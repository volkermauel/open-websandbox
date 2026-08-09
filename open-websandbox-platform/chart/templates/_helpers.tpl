{{/*
Expand the chart name.
*/}}
{{- define "open-websandbox.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Standard labels applied to every open-websandbox resource. Workload selectors and
pod-template labels keep the source manifests' own distinguishing labels
(e.g. app.kubernetes.io/name: owui-broker / sandbox-router) so selectors remain
stable and chart-managed labels never collide with them.
*/}}
{{- define "open-websandbox.labels" -}}
app.kubernetes.io/name: open-websandbox
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version | replace "+" "_" }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: open-websandbox
{{- end -}}

{{/*
System (control-plane) namespace — broker + router.
*/}}
{{- define "open-websandbox.systemNamespace" -}}
{{- .Values.namespaces.system -}}
{{- end -}}

{{/*
Runtime (sandbox pods) namespace — SandboxTemplate/WarmPool, quota, NP, PVC.
*/}}
{{- define "open-websandbox.runtimeNamespace" -}}
{{- .Values.namespaces.runtime -}}
{{- end -}}

{{/*
Container image: <registry>/<owner>/<repo>:<tag>, or bare <repo>:<tag> when the
registry is empty (dev / hand-loaded images).

Usage:
  {{ include "open-websandbox.image" (dict "Values" .Values "repo" "open-websandbox-broker") }}
*/}}
{{- define "open-websandbox.image" -}}
{{- if .Values.imageRegistry -}}
{{- printf "%s/%s/%s:%s" .Values.imageRegistry .Values.imageOwner .repo .Values.imageTag -}}
{{- else -}}
{{- printf "%s:%s" .repo .Values.imageTag -}}
{{- end -}}
{{- end -}}

{{/*
Broker shared secret resolution.
- If broker.sharedSecret is set (non-empty), use it verbatim.
- Else, if the owui-broker-secret Secret already exists (real cluster, e.g. an upgrade),
  2read it back and preserve it — so the value is stable across `helm upgrade`.
- Else (first install, or `helm template`/`lint` with no cluster), generate a random
  48-char secret.
Never returns the legacy dev placeholder. Pair with values.schema.json, which also
forbids the placeholder.
*/}}
{{- define "open-websandbox.brokerSharedSecret" -}}
{{- $provided := .Values.broker.sharedSecret -}}
{{- if $provided -}}
{{- $provided -}}
{{- else -}}
{{- $existing := lookup "v1" "Secret" (include "open-websandbox.systemNamespace" .) "owui-broker-secret" -}}
{{- if and $existing (hasKey $existing.data "shared-secret") -}}
{{- index $existing.data "shared-secret" | b64dec -}}
{{- else -}}
{{- randAlphaNum 48 -}}
{{- end -}}
{{- end -}}
{{- end -}}
