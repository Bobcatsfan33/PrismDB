{{- define "prism-shard.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "prism-shard.fullname" -}}
{{- printf "%s-%s" .Release.Name (include "prism-shard.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "prism-shard.labels" -}}
app.kubernetes.io/name: {{ include "prism-shard.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end -}}

{{/*
Refuse a mutable image reference.

The JSON schema already requires an immutable `sha256:` digest and forbids an `image.tag` key, but
a repository string carrying its own `:tag` or `@sha256:` would slip past a per-field pattern and
silently win at pull time. Rendering must fail rather than produce a manifest whose bytes are
decided later by whoever last pushed the tag.
*/}}
{{- define "prism-shard.image" -}}
{{- $repository := .Values.image.repository -}}
{{- if regexMatch "[:@]" $repository -}}
{{- fail (printf "image.repository %q must not carry a tag or digest; pin bytes through image.digest" $repository) -}}
{{- end -}}
{{- printf "%s@%s" $repository .Values.image.digest -}}
{{- end -}}

{{/*
Refuse one bundle serving two trust roles.

The shard's server identity answers "this is the shard"; the coordinator CA answers "this caller may
reach the shard". One Secret behind both means any coordinator certificate can also impersonate a
shard endpoint. `prism shard-serve` refuses the shared bundle at startup; failing here means the
operator learns it from `helm template`, not from a crash loop.
*/}}
{{- define "prism-shard.assertDistinctTrust" -}}
{{- if eq .Values.secrets.serverTls .Values.secrets.coordinatorCa -}}
{{- fail (printf "secrets.serverTls and secrets.coordinatorCa are both %q; the shard-server identity and the coordinator-client trust must be separate Secrets" .Values.secrets.serverTls) -}}
{{- end -}}
{{- end -}}

{{/*
The number of writers is the number of shards.

Replicas are derived from the topology rather than configured beside it, so "exactly one writer per
shard" is arithmetic, not a convention an operator can drift from by editing one value.
*/}}
{{- define "prism-shard.replicas" -}}
{{- len .Values.topology.shards -}}
{{- end -}}
