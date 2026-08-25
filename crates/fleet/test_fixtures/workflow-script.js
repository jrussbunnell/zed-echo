export const meta = {
  name: 'alpha-beta',
  description: 'Two-phase smoke test: one agent per phase',
  phases: [
    { title: 'Alpha', detail: 'single agent replies ALPHA_DONE' },
    { title: 'Beta', detail: 'single agent replies BETA_DONE' },
  ],
}

phase('Alpha')
const alpha = await agent('Reply with exactly the word ALPHA_DONE and nothing else. Do not use any tools.', { label: 'alpha', phase: 'Alpha' })

phase('Beta')
const beta = await agent('Reply with exactly the word BETA_DONE and nothing else. Do not use any tools.', { label: 'beta', phase: 'Beta' })

return { alpha, beta }
