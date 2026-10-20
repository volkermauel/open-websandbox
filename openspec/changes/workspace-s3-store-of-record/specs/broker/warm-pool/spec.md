# broker/warm-pool — VCT scratch volumes

## ADDED Requirements

### Requirement: pool-template scratch volumes

When `warmPool.scratch.enabled` is true, the pool's SandboxTemplate SHALL
declare a `volumeClaimTemplates` entry that overrides the template's
same-named `emptyDir` workspace volume, so every pool member (and every
sandbox adopted from the pool) gets a controller-managed per-sandbox PVC.

#### Scenario: warm member carries a PVC

- **WHEN** a pool member Sandbox is created from the template
- **THEN** a PVC `<templateName>-<sandboxName>` exists (bound before the pod
  goes Ready), and the pod's workspace volume resolves to that PVC

#### Scenario: adoption keeps data

- **WHEN** a claim adopts a warm member
- **THEN** the PVC and its data follow the Sandbox for the chat's lifetime

#### Scenario: reap garbage-collects

- **WHEN** the reaper deletes an ephemeral chat's claim
- **THEN** the kube garbage collector removes the Sandbox and the PVC
  (ownerRef chain) without broker involvement

#### Scenario: claims stay warm-eligible

- **WHEN** the broker creates an ephemeral claim
- **THEN** the claim carries no `env` and no claim-level
  `volumeClaimTemplates` (both force cold adoption upstream), so warm
  binding continues to work

#### Scenario: persistent profile untouched

- **WHEN** the profile is `Persistent`
- **THEN** broker-managed per-user PVCs + subPaths + S3 tiering behave
  exactly as before (template VCTs never apply to the persistent template)
