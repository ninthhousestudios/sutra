# sutra — code intelligence sketch

Status: sketch
Date: 2026-04-27
Context: modular ecosystem for filesystem (smriti) and documents (vedakosha)

## what it is

Sutra (सूत्र — "thread," "string," or "concise rule") is a modular code intelligence tool. It focuses on the **structure** and **logic** of source code through Abstract Syntax Tree (AST) analysis.

If Smriti is the "memory" that knows a file exists and where it has been, Sutra is the "reasoning" that understands what the code inside that file actually *does*.

## why "sutra"?

The name reflects the dual nature of code:
1.  **Thread:** Code is a sequence of logical threads that weave together a system.
2.  **Aphorism:** Like a traditional sutra, code is often terse, cryptic to the uninitiated, but contains precisely encoded logic for those (and the tools) who know how to parse it.

## core philosophy

- **Structure over Text:** Moves beyond `grep` to understand scope, definitions, and true references.
- **AST-First:** Built on top of tree-sitter or similar robust parsers to handle language-specific idioms.
- **Modularity:** Distinct from document search (**Vedakosha**). Code requires different indexing strategies than natural language; Sutra does not rely on embeddings where structural logic is more reliable.
- **Agent-Native:** Designed to give AI agents (and human devs) high-fidelity "perception" of a codebase's architecture.

## relation to the ecosystem

```
smriti (filesystem perception)
  │ "src/main.rs has changed (hash: ab12...)"
  v
sutra (code intelligence)
  │ parse AST, update symbol table, trace call graphs
  │ "symbol 'Scanner' moved to src/scanner.rs"
  v
vedakosha (document knowledge)
  │ "find documents explaining the 'Scanner' architecture"
```

## expected capabilities

- **Symbol Navigation:** Jump to definition, find all references (scope-aware).
- **Call Hierarchy:** Trace callers and callees without the noise of string matching.
- **Blast Radius Analysis:** Understand how changing a function signature impacts the rest of the system.
- **Refactoring Safety:** Perform renames and moves by updating the AST nodes, ensuring logical consistency.
- **Complexity Metrics:** Measure cyclomatic complexity and "messiness" to identify refactor candidates.

## timeline

Modular implementation follows the stabilization of Smriti's core scanner and event system. Sutra will likely be the primary consumer of Smriti's "code" tier events.
