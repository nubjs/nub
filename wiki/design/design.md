# Design

Mechanism documents: how Nub is put together and what a change should be designed against. The investigations that supply the evidence for these decisions live under [[research]].

- [[architecture]] — Nub as an augmenter of the user's installed Node, and the Node-published surfaces every augmentation reaches the process through
- [[compat-mode-tests]] — the four behaviors `--node` and `NODE_COMPAT` guarantee, each mapped to the test that proves it
- [[compiled-executables]] — how `nub compile` assembles a single executable that runs with no Node and no `node_modules` on the target machine
- [[install-script-failure-readout]] — proposed output for reporting which dependency install scripts failed, why, and what to do next
