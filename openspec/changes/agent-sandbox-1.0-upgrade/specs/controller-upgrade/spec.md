# controller-upgrade — agent-sandbox v1.0.7

## MODIFIED Requirement: vendored upstream controller pin

The chart SHALL vendor agent-sandbox **v1.0.7** (CRDs + controller slice,
verbatim from the SHA-256-recorded release asset). The v1alpha1 served
version and the conversion webhook no longer exist in the vendored slice.

### Scenario: CRD-first apply

- **WHEN** the chart (or the ops runbook) upgrades the controller
- **THEN** the four CRDs are applied before the controller Deployment, and
  every CRD reports `status.storedVersions == ["v1beta1"]` before the new
  controller starts

### Scenario: webhook-free slice

- **WHEN** the controller slice is rendered or applied
- **THEN** no webhook Service, webhook-cert Role/RoleBinding, or
  `customresourcedefinitions` verbs appear; any leftover webhook objects
  from a v0.5.6 install are deleted as a post-upgrade step

### Scenario: broker compatibility

- **WHEN** the upgraded controller reconciles Sandboxes, claims, and warm
  pools driven by the existing broker
- **THEN** no broker code change is required (group/version unchanged,
  additive schema only) and existing resolve/park/claim flows behave as
  before, with the v1.0.7 `NotSuspended` condition semantics
