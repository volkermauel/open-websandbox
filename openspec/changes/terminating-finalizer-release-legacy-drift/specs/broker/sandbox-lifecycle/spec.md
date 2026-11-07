# broker/sandbox-lifecycle — template-drift self-heal

## MODIFIED Requirement: drift detection without the hash stamp (legacy)

Drift detection SHALL NOT require the `owui.io/template-hash` annotation.
The template a sandbox is compared against SHALL be resolved in priority
order: `owui.io/template-name`, then the controller-stamped
`agents.x-k8s.io/sandbox-template-ref`, then the configured base template.

When the hash stamp is absent, drift SHALL be decided by canonical-JSON
equality of the sandbox's ACTUAL `spec.podTemplate` against the live
template's `spec.podTemplate`: equal ⇒ no drift, different ⇒ drifted. A
sandbox SHALL never be recycled when the template cannot be read (missing,
apiserver error, no podTemplate) or when the sandbox carries no copied
podTemplate (controller-built / claim-bound sandboxes). The hash-stamp path
keeps priority and its semantics; the idle-only guard
(`recycle_eligible`), the 120 s `broker-recycled-at` grace and the
2-per-tick reaper cap are unchanged. Applied on BOTH the resolve path and
the reaper path.

### Scenario: legacy sandbox with a stale copied podTemplate is recycled

- **WHEN** a sandbox has no hash annotation, its copied `spec.podTemplate`
  differs from the live template's, and the reaper's idle predicate says
  nothing is using it
- **THEN** it is recycled through the cold-create path (fresh hash +
  grace stamp), on resolve and on the reaper tick alike

### Scenario: legacy sandbox matching the live template keeps serving

- **WHEN** a sandbox has no hash annotation and its copied
  `spec.podTemplate` equals the live template's (key-order free)
- **THEN** no drift is reported and the sandbox keeps serving

### Scenario: unreadable template never recycles

- **WHEN** the resolved template is missing, unreadable, or has no
  podTemplate — or the sandbox has no copied podTemplate to compare
- **THEN** the sandbox is left untouched
