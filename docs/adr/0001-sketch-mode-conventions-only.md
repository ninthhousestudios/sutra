# Sketch mode relaxes conventions only, not constraints

> **Superseded (2026-08-10, sutra/318).** Sketch mode has been removed. Its only
> effect was flattening convention lifecycle states to informational in the
> review deviation report; that report was removed (sutra/313), leaving the
> `components.lifecycle_state` column with no reader or writer, so it was dropped
> (migration 0060). This ADR is retained as a decision record. The reasoning
> below (constraints stay enforced through experimentation; conventions are the
> relaxable layer) remains sound and would apply if a sketch-like mode is
> rebuilt.

When a component is in sketch mode (active prototyping), all convention
lifecycle states flatten to informational — conventions are tracked but not
enforced. Constraints remain fully enforced regardless of component lifecycle
state.

The alternative was relaxing everything — making constraints advisory in
sketch mode too. We rejected this because violating a constraint during a
spike can invalidate the spike's conclusions. If the final product must
respect a boundary (e.g., "db must not import http"), proving something works
by ignoring that boundary proves nothing useful. Conventions are different —
they're about patterns, and during experimentation you're actively searching
for better patterns.
