Nub's public design and research corpus, structured as a [lat.md](https://github.com/1st1/lat.md) knowledge graph — cross-linked markdown that records what the project does and why, with `lat check` holding every link and code reference to something that still exists.

The corpus lives in `wiki/`. A `lat.md` symlink at the repo root points here, because `lat` finds its graph by that directory name; this file is the graph's root index, and its name is fixed by the same rule. Roadmap and per-command planning material is deliberately not here.

- [[agents]] — the entry point for any agent working in this repo: the non-negotiables, the core design positions, and the workflow rules
- [[design]] — architecture and the design decisions a change should be built against
- [[research]] — the investigations behind those decisions: measured results, ecosystem surveys, and what settled a choice one way rather than another
