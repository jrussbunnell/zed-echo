//! A GPUI port of Otto's `voice-glow.tsx` (itself a port of Libraries.dev's
//! `VoiceBeam`, MIT): seven soft lobes fanned from the bottom centre, rising
//! with the level and flowing sideways while someone talks. Input lobes are
//! teal and blue, output lobes violet, so the colour says who is speaking.

use crate::prelude::*;
use gpui::{Bounds, BoxShadow, ContentMask, Hsla, Pixels, canvas, hsla, point, size};
use std::time::Instant;

#[derive(Clone, Copy)]
enum Voice {
    Input,
    Output,
}

struct Lobe {
    x: f32,
    width: f32,
    height: f32,
    band: usize,
    voice: Voice,
    color: Hsla,
}

const TEAL: Hsla = hsla(0.47, 0.65, 0.48, 1.);
const LIGHT_TEAL: Hsla = hsla(0.47, 0.6, 0.6, 1.);
const SKY: Hsla = hsla(0.55, 0.8, 0.58, 1.);
const BLUE: Hsla = hsla(0.61, 0.75, 0.62, 1.);
const VIOLET: Hsla = hsla(0.74, 0.6, 0.66, 1.);
const DEEP_VIOLET: Hsla = hsla(0.74, 0.55, 0.55, 1.);
const FUCHSIA: Hsla = hsla(0.84, 0.65, 0.64, 1.);

const LOBES: [Lobe; 7] = [
    lobe(0., 74., 46., 0, Voice::Input, TEAL),
    lobe(-36., 54., 40., 1, Voice::Output, VIOLET),
    lobe(36., 54., 40., 1, Voice::Input, SKY),
    lobe(-72., 48., 32., 2, Voice::Output, FUCHSIA),
    lobe(72., 48., 32., 2, Voice::Input, BLUE),
    lobe(-108., 42., 26., 0, Voice::Output, DEEP_VIOLET),
    lobe(108., 42., 26., 1, Voice::Input, LIGHT_TEAL),
];

const fn lobe(x: f32, width: f32, height: f32, band: usize, voice: Voice, color: Hsla) -> Lobe {
    Lobe {
        x,
        width,
        height,
        band,
        voice,
        color,
    }
}

/// The library's design width; every lobe measure is relative to it.
pub const DESIGN_WIDTH: f32 = 350.;
const SPAN: f32 = 36. * LOBES.len() as f32;
const HALF: f32 = SPAN / 2.;

/// Speech RMS sits well under half of full scale. This is the calibration
/// knob: raise it if the glow looks sleepy, lower it if it pins at full height.
pub const GAIN: f32 = 2.5;

/// A level is one number, not a spectrum, so the three "bands" are the same
/// signal followed at three speeds, which keeps neighbouring lobes from moving
/// in lockstep.
const ATTACK: [f32; 3] = [0.06, 0.09, 0.13];
const RELEASE: [f32; 3] = [0.3, 0.4, 0.55];

/// (blur in design pixels, opacity) for the wide bloom and the tighter core.
const LAYERS: [(f32, f32); 2] = [(22., 0.95), (8., 0.9)];

pub fn follow(current: f32, target: f32, dt: f32, attack: f32, release: f32) -> f32 {
    let tau = if target > current { attack } else { release };
    current + (target - current) * (1. - (-dt / tau).exp())
}

/// Where one lobe sits and how it is stretched: `x` is the offset of its
/// centre from the element's centre in pixels, and the scales multiply its
/// resting size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LobePaint {
    pub x: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub opacity: f32,
}

pub fn lobe_paint(
    lobe_index: usize,
    phase: f32,
    bands_input: [f32; 3],
    bands_output: [f32; 3],
    elapsed: f32,
    scale: f32,
) -> LobePaint {
    let Some(lobe) = LOBES.get(lobe_index) else {
        return LobePaint {
            x: 0.,
            scale_x: 1.,
            scale_y: 1.,
            opacity: 0.,
        };
    };
    let level = bands_input[1].max(bands_output[1]);
    let idle = 0.3 + 0.06 * (elapsed * (2. * std::f32::consts::PI / 5.2)).sin();
    let spread = 1. + 0.6 * level;
    let x = (((lobe.x + phase + HALF) % SPAN) + SPAN) % SPAN - HALF;
    let edge = 1. - (x.abs() / HALF).powi(4);
    let bands = match lobe.voice {
        Voice::Input => bands_input,
        Voice::Output => bands_output,
    };
    let energy = idle.max(bands.get(lobe.band).copied().unwrap_or(0.));
    LobePaint {
        x: x * spread * scale,
        scale_x: 1. + 0.6 * energy,
        scale_y: 0.3 + 1.5 * energy,
        opacity: edge * (0.3 + 0.7 * (energy * 1.4).min(1.)),
    }
}

#[derive(Clone, Copy, Default)]
struct GlowState {
    input: [f32; 3],
    output: [f32; 3],
    phase: f32,
    elapsed: f32,
    last_frame: Option<Instant>,
}

impl GlowState {
    fn advance(&mut self, input_level: f32, output_level: f32, dt: f32) {
        let input_target = (input_level * GAIN).min(1.);
        let output_target = (output_level * GAIN).min(1.);
        for band in 0..3 {
            self.input[band] = follow(
                self.input[band],
                input_target,
                dt,
                ATTACK[band],
                RELEASE[band],
            );
            self.output[band] = follow(
                self.output[band],
                output_target,
                dt,
                ATTACK[band],
                RELEASE[band],
            );
        }
        let level = self.input[1].max(self.output[1]);
        self.phase = (self.phase + dt * 80. * level) % SPAN;
        self.elapsed += dt;
    }
}

/// The voice glow. Levels are plain numbers (0 to 1) so this crate needs no
/// audio types; the caller reads them each frame.
#[derive(IntoElement)]
pub struct VoiceGlow {
    id: ElementId,
    input_level: f32,
    output_level: f32,
}

impl VoiceGlow {
    pub fn new(id: impl Into<ElementId>, input_level: f32, output_level: f32) -> Self {
        Self {
            id: id.into(),
            input_level,
            output_level,
        }
    }
}

impl RenderOnce for VoiceGlow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let reduce_motion = cx.reduce_motion();
        let state = window.use_keyed_state(self.id, cx, |_, _| GlowState::default());
        // Reduced motion keeps the untouched resting shape.
        let glow = state.update(cx, |state, _| {
            if !reduce_motion {
                let now = Instant::now();
                let dt = state
                    .last_frame
                    .map(|last| now.duration_since(last).as_secs_f32().min(0.05))
                    .unwrap_or(1. / 60.);
                state.last_frame = Some(now);
                state.advance(self.input_level, self.output_level, dt);
            }
            *state
        });
        if !reduce_motion {
            window.request_animation_frame();
        }
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| paint_lobes(bounds, &glow, window),
        )
        .size_full()
    }
}

fn paint_lobes(bounds: Bounds<Pixels>, glow: &GlowState, window: &mut Window) {
    let width = f32::from(bounds.size.width);
    let scale = width / DESIGN_WIDTH;
    let center_x = f32::from(bounds.center().x);
    let bottom = f32::from(bounds.bottom());
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        for (blur, layer_opacity) in LAYERS {
            for (index, lobe) in LOBES.iter().enumerate() {
                let paint = lobe_paint(
                    index,
                    glow.phase,
                    glow.input,
                    glow.output,
                    glow.elapsed,
                    scale,
                );
                let lobe_width = lobe.width * 1.25 * scale * paint.scale_x;
                let lobe_height = lobe.height * 2.9 * scale * paint.scale_y;
                // A radial gradient fading to nothing, approximated by a
                // shadow blurred over a quarter of the lobe on top of the
                // layer's own blur.
                let blur_radius = blur * scale + lobe_width.min(lobe_height) * 0.25;
                let lobe_bounds = Bounds::new(
                    point(
                        px(center_x + paint.x - lobe_width / 2.),
                        px(bottom - lobe_height / 2.),
                    ),
                    size(px(lobe_width), px(lobe_height)),
                );
                window.paint_drop_shadows(
                    lobe_bounds,
                    gpui::Corners::all(px(lobe_width.min(lobe_height) / 2.)),
                    &[BoxShadow {
                        color: lobe.color.opacity(paint.opacity * layer_opacity),
                        offset: point(px(0.), px(0.)),
                        blur_radius: px(blur_radius),
                        spread_radius: px(0.),
                        inset: false,
                    }],
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-5
    }

    #[test]
    fn follow_attacks_faster_than_it_releases() {
        let rising = follow(0., 1., 0.05, ATTACK[0], RELEASE[0]);
        let falling = follow(1., 0., 0.05, ATTACK[0], RELEASE[0]);
        assert!(rising > 0. && rising < 1.);
        assert!(1. - falling < rising, "rose {rising}, fell to {falling}");
        // 1 - e^(-0.05/0.06)
        assert!(close(rising, 0.565402));
    }

    #[test]
    fn lobe_paint_matches_the_typescript_original() {
        // Lobe 0 sits at the centre and follows input band 0, which at 0.5
        // beats the idle breath of 0.3. Level 0.5 gives a spread of 1.3.
        let centre = lobe_paint(0, 0., [0.5; 3], [0.; 3], 0., 1.);
        assert!(close(centre.x, 0.), "{centre:?}");
        assert!(close(centre.scale_x, 1.3), "{centre:?}");
        assert!(close(centre.scale_y, 1.05), "{centre:?}");
        assert!(close(centre.opacity, 0.79), "{centre:?}");

        // Lobe 1 sits at x = -36 and follows output band 1, which is silent,
        // so the idle breath (0.3) drives it: x = -36 * 1.3, edge fade
        // 1 - (36/126)^4, opacity edge * (0.3 + 0.7 * 0.42).
        let left = lobe_paint(1, 0., [0.5; 3], [0.; 3], 0., 1.);
        assert!(close(left.x, -46.8), "{left:?}");
        assert!(close(left.scale_x, 1.18), "{left:?}");
        assert!(close(left.scale_y, 0.75), "{left:?}");
        assert!(close(left.opacity, 0.590_041_6), "{left:?}");

        let scaled = lobe_paint(1, 0., [0.5; 3], [0.; 3], 0., 2.);
        assert!(close(scaled.x, -93.6), "{scaled:?}");
    }
}
