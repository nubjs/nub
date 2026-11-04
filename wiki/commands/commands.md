# Commands

How an individual Nub command is assembled: the stages it runs, the artifacts it produces, and the invariants a change to it has to preserve. Where [[design]] records what the project decided, these documents record how one command carries that decision out.

A command earns a document here once its implementation is large enough that reading the source no longer tells you the shape — where the work spans several crates, or where the order of two stages is itself the design. Roadmap and per-command planning material is deliberately not here.

- [[compile-architecture]] — the `nub compile` pipeline: how an entry file becomes a launcher carrying an embedded payload, and what the launcher does with it at startup
