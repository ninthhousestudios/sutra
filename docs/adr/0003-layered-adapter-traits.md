# Language adapters use layered traits, not a single wide interface

Language support is structured as one required trait (parse: produce symbols
and edges from a tree-sitter tree) plus optional extension traits for
higher-layer concerns (FCA attribute enrichment, verification tool
integration, etc.). A language adapter implements what it can; the core
queries capabilities at runtime.

We rejected a single wide trait because it forces every language to stub out
methods it can't support, and the surface grows with every new analysis. We
rejected a narrow parse-only interface because it forecloses
language-specific richness — Rust's type system, trait impls, and visibility
modifiers are valuable FCA attributes that a generic symbol/edge model can't
express.

The layered approach matches the vision's "capability levels" concept: each
adapter declares what it supports, and the core adapts gracefully. A new
language starts with just the parse trait and immediately works for structural
analysis. Richer capabilities come incrementally as extension traits are
implemented.
