{{- define "prismdb.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "prismdb.fullname" -}}
{{- printf "%s-%s" .Release.Name (include "prismdb.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "prismdb.labels" -}}
app.kubernetes.io/name: {{ include "prismdb.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end -}}
