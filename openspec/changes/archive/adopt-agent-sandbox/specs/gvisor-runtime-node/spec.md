## ADDED Requirements

### Requirement: A cluster-wide gVisor RuntimeClass shall exist

The system SHALL provide a Kubernetes `RuntimeClass` named `gvisor` with handler
`runsc`, applied cluster-wide, so any pod that sets `runtimeClassName: gvisor`
runs under gVisor on any node that has the runsc handler.

#### Scenario: A pod opts into gVisor

- **WHEN** a pod with `runtimeClassName: gvisor` is scheduled on a worker that
  has the runsc handler
- **THEN** it SHALL execute under the gVisor userspace kernel, verifiable by
  `uname -r` reporting `4.19.0-gvisor` and `dmesg` reporting "Starting gVisor...".

### Requirement: runsc shall be installed via the containerd template, online-safe

The `runsc` handler SHALL be injected through the snap-MicroK8s
`containerd-template.toml` (not the regenerated `containerd.toml`), and the
change SHALL be inert until the `snap.microk8s.daemon-containerd` service is
restarted. The activation procedure SHALL cordon the node, restart containerd,
confirm the handler is present in the rendered config, confirm no running pod was
disrupted, probe gVisor, and uncordon.

#### Scenario: Adding gVisor to a live worker causes no disruption

- **WHEN** gVisor is staged and activated on a worker that hosts running pods
- **THEN** the staging edit SHALL NOT restart containerd, and the activation
  restart SHALL leave every previously-running pod in a `Running`/`Completed`
  state with no new restarts attributable to the restart.

### Requirement: A CNPG primary shall be failed over before node maintenance

The operator SHALL, before restarting containerd on (or draining) a node that
hosts a CloudNativePG primary, perform a controlled switchover
(`kubectl cnpg promote <cluster> <replica-on-another-node>`) to a healthy replica
on another node, so no primary is at risk during the maintenance window.
Replica-only nodes need no failover.

#### Scenario: Maintenance on a primary-holding node

- **WHEN** containerd must be restarted on a node hosting a CNPG primary
- **THEN** the primary SHALL first be switched to a healthy replica on another
  node, verified streaming with negligible replay lag, before the restart begins.
