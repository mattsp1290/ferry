{{/* Release-scoped resource name. */}}
{{- define "ferry.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/* Value of DD_VERSION and the version label. */}}
{{- define "ferry.version" -}}
{{- default .Chart.AppVersion .Values.image.version -}}
{{- end -}}

{{/* Labels for every object. */}}
{{- define "ferry.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | quote }}
{{- end -}}

{{/* Selector labels. */}}
{{- define "ferry.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Fails the render on values the chart cannot work with. */}}
{{- define "ferry.validate" -}}
{{- $_ := required "image.repository is required (for example registry.example.internal/ferry/ferry)" .Values.image.repository -}}
{{- $_ := required "image.digest is required: pin the image by digest (sha256:...), never by tag" .Values.image.digest -}}
{{- if not (has .Values.datadog.transport (list "socket" "service" "none")) -}}
{{- fail (printf "datadog.transport must be one of socket, service, none; got %q" (toString .Values.datadog.transport)) -}}
{{- end -}}
{{- end -}}

{{/*
TOML rendering. Helm hands values-file numbers to templates as float64, and
toToml then writes `poll_interval_seconds = 300.0`, which ferry's parser
rejects for its integer fields. These helpers render whole-number floats as
integers instead. Output lines each start with a newline; callers trim.
*/}}

{{/* One TOML value: string, bool, integer, float, or array of those. */}}
{{- define "ferry.toml.scalar" -}}
{{- if kindIs "string" . -}}
{{- . | quote -}}
{{- else if kindIs "bool" . -}}
{{- . -}}
{{- else if kindIs "float64" . -}}
{{- if eq (floor .) . -}}{{- printf "%d" (int64 .) -}}{{- else -}}{{- printf "%v" . -}}{{- end -}}
{{- else if kindIs "slice" . -}}
[{{- range $i, $e := . -}}{{- if $i -}}, {{ end -}}{{- include "ferry.toml.scalar" $e -}}{{- end -}}]
{{- else if kindIs "invalid" . -}}
{{- fail "config contains a null value, which TOML cannot represent" -}}
{{- else -}}
{{- printf "%d" (int64 .) -}}
{{- end -}}
{{- end -}}

{{/* "true" for a non-empty list of maps, which TOML writes as [[array]] tables. */}}
{{- define "ferry.toml.isTables" -}}
{{- if and (kindIs "slice" .) (gt (len .) 0) (kindIs "map" (index . 0)) -}}true{{- end -}}
{{- end -}}

{{/* A table body: scalars, then sub-tables, then arrays of tables. Takes (dict "path" "a.b" "data" map). */}}
{{- define "ferry.toml.table" -}}
{{- $path := .path -}}
{{- $data := .data -}}
{{- range $k := keys $data | sortAlpha -}}
{{- $v := get $data $k -}}
{{- if not (or (kindIs "map" $v) (eq (include "ferry.toml.isTables" $v) "true")) }}
{{ $k }} = {{ include "ferry.toml.scalar" $v }}
{{- end -}}
{{- end -}}
{{- range $k := keys $data | sortAlpha -}}
{{- $v := get $data $k -}}
{{- $sub := ternary $k (printf "%s.%s" $path $k) (eq $path "") }}
{{- if kindIs "map" $v }}

[{{ $sub }}]
{{- include "ferry.toml.table" (dict "path" $sub "data" $v) -}}
{{- else if eq (include "ferry.toml.isTables" $v) "true" -}}
{{- range $item := $v }}

[[{{ $sub }}]]
{{- include "ferry.toml.table" (dict "path" $sub "data" $item) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* The whole ferry.toml. */}}
{{- define "ferry.toml" -}}
{{- include "ferry.toml.table" (dict "path" "" "data" .Values.config) | trim -}}
{{- end -}}
