{{/*
Expand the chart name.
*/}}
{{- define "open-sandbox.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Standard labels applied to every open-sandbox resource. Workload selectors and
pod-template labels keep the source manifests' own distinguishing labels
(e.g. app.kubernetes.io/name: owui-broker / sandbox-router) so selectors remain
stable and chart-managed labels never collide with them.
*/}}
{{- define "open-sandbox.labels" -}}
app.kubernetes.io/name: open-sandbox
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version | replace "+" "_" }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: open-sandbox
{{- end -}}

{{/*
System (control-plane) namespace — broker + router.
*/}}
{{- define "open-sandbox.systemNamespace" -}}
{{- .Values.namespaces.system -}}
{{- end -}}

{{/*
Runtime (sandbox pods) namespace — SandboxTemplate/WarmPool, quota, NP, PVC.
*/}}
{{- define "open-sandbox.runtimeNamespace" -}}
{{- .Values.namespaces.runtime -}}
{{- end -}}

{{/*
Container image: <registry>/<owner>/<repo>:<tag>, or bare <repo>:<tag> when the
registry is empty (dev / hand-loaded images).

Usage:
  {{ include "open-sandbox.image" (dict "Values" .Values "repo" "open-sandbox-broker") }}
*/}}
{{- define "open-sandbox.image" -}}
{{- if .Values.imageRegistry -}}
{{- printf "%s/%s/%s:%s" .Values.imageRegistry .Values.imageOwner .repo .Values.imageTag -}}
{{- else -}}
{{- printf "%s:%s" .repo .Values.imageTag -}}
{{- end -}}
{{- end -}}
