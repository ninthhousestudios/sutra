# Sketch mode relaxes conventions only, not constraints

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
