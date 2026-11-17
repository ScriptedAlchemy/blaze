---
status: plan
summary: Charge Hegel's TooSlow generation health check only for generator work, including custom generators, nesting, rejection and clone streams.
date: 2026-11-12
---
# Generation time and property body time

Three native linker properties fail Hegel's TooSlow guard when executed individually:
4 valid cases after33.425 seconds, 2 after96.395, and 2 after116.626. The runner's
`test_function_tagged` times the complete execution and `record_run` charges it to a
check explicitly described as input-generation time. A property with cheap draws
and an expensive correctness oracle is therefore misclassified as a slow generator.

Keep the existing threshold, case counts, rejection checks and shrink limits. Record
whole-body elapsed time separately. Count the union of generation intervals shared
by all streams of a test case. The Rust client opens an unwind-safe generation scope
around each public draw entry point, so custom generators, mappings, filters and
printing count too. Nested draws count once. Native data-source operations also open
scopes so direct native/C clients retain generation accounting. Time between draws
belongs to the body. Closing a draw scope must remain legal after an assumption or
another clone concludes the family. Validate per-stream scope pairing; a closing
scope on one stream must not consume another stream's scope.

Use a small synchronous interval counter with injected monotonic times, protected by
the existing engine mutex in production. Exercise nested and overlapping intervals,
all bounded interleavings, invalid closes, panics and rejected draws. The runner must
accept an expensive body with cheap generation, and still refuse expensive generation,
using injected durations rather than a30-second sleep. Preserve existing wall-time
statistics and execution budgets. The timer state is transient; no persisted choice,
replay or serialized record changes. The paired scope operation is internal to the
vendored static Rust client/engine build.

Lowering case counts, raising the threshold, or suppressing TooSlow was rejected:
none makes the generation health check measure generation. Run these regressions
with Blaze, then rerun the three unchanged linker properties and full suite on a
combined tree. Do not claim the accounting change makes the linker itself faster.
