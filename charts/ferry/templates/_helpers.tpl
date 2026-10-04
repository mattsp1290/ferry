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

{{/* Shared runtime pod fragments used by Deployment and preflight. */}}
{{- define "ferry.podSecurityContext" -}}
# runAsUser, runAsGroup, and runAsNonRoot are set per container: the
# optional fix-permissions initContainer must run as root.
fsGroup: 10001
seccompProfile:
  type: RuntimeDefault
{{- end -}}

{{- define "ferry.fixPermissionsContainer" -}}
- name: fix-permissions
  image: {{ (printf "%s@%s" .Values.image.repository .Values.image.digest) | quote }}
  imagePullPolicy: {{ .Values.image.pullPolicy }}
  # local-path volumes are hostPath-backed and ignore fsGroup. The
  # mount point is enough: ferry creates everything below it.
  command: ["chown", "10001:10001", "/var/lib/ferry"]
  securityContext:
    runAsNonRoot: false
    runAsUser: 0
    runAsGroup: 0
    readOnlyRootFilesystem: true
    allowPrivilegeEscalation: false
    capabilities:
      drop: ["ALL"]
      add: ["CHOWN"]
  resources:
    requests: { cpu: 10m, memory: 16Mi }
    limits: { cpu: 100m, memory: 64Mi }
  volumeMounts:
    - name: cache
      mountPath: /var/lib/ferry
{{- end -}}

{{- define "ferry.containerSecurityContext" -}}
runAsNonRoot: true
runAsUser: 10001
runAsGroup: 10001
readOnlyRootFilesystem: true
allowPrivilegeEscalation: false
capabilities:
  drop: ["ALL"]
{{- end -}}

{{- define "ferry.runtimeMounts" -}}
- name: credentials
  mountPath: /var/run/secrets/ferry
  readOnly: true
- name: cache
  mountPath: /var/lib/ferry
- name: tmp
  mountPath: /tmp
{{- if eq .Values.datadog.transport "socket" }}
- name: datadog-socket
  mountPath: /var/run/datadog
  readOnly: true
{{- end }}
{{- end -}}

{{- define "ferry.runtimeVolumes" -}}
- name: credentials
  secret:
    secretName: {{ .Values.credentialsSecret | quote }}
    # 0440 octal. Written in decimal because YAML 1.1 and 1.2 parsers
    # disagree on a leading zero. Kubernetes projects Secret files as
    # root:<fsGroup>, so UID 10001 reads them through the group.
    # Without an items list every key present is projected, so a
    # missing github-token key just means no file.
    defaultMode: 288
- name: cache
  persistentVolumeClaim:
    claimName: {{ default (printf "%s-cache" (include "ferry.fullname" .)) .cacheClaim }}
- name: tmp
  emptyDir: {}
{{- if eq .Values.datadog.transport "socket" }}
- name: datadog-socket
  hostPath:
    path: {{ .Values.datadog.socketHostPath | quote }}
    type: Directory
{{- end }}
{{- end -}}
