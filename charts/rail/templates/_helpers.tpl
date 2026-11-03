{{/* Resource name for an instance: <release>-<instance>. Call with (dict "root" $ "name" "edge"). */}}
{{- define "rail.instanceName" -}}
{{- printf "%s-%s" .root.Release.Name .name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* In-cluster base URL of an instance: what other instances and the test pod call. */}}
{{- define "rail.serviceUrl" -}}
{{- printf "http://%s.%s.svc.%s:%v" (include "rail.instanceName" .) .root.Release.Namespace .root.Values.clusterDomain .root.Values.port -}}
{{- end -}}

{{/*
Public base URL of an instance: what its short links are minted under, so what browsers follow.
With ingress enabled every instance gets <instance>.<host>, because a redirect chain passes
through every tier, not only the top one.
*/}}
{{- define "rail.publicUrl" -}}
{{- $ingress := .root.Values.ingress -}}
{{- if $ingress.enabled -}}
{{- printf "%s://%s.%s" (ternary "https" "http" $ingress.tls.enabled) .name $ingress.host -}}
{{- else -}}
{{- include "rail.serviceUrl" . -}}
{{- end -}}
{{- end -}}

{{/* RAIL_UPSTREAMS for an instance, expanding rail:<name> into a full URL. */}}
{{- define "rail.upstreams" -}}
{{- $root := .root -}}
{{- $self := .instance.name -}}
{{- $known := list -}}
{{- range $root.Values.instances }}{{ $known = append $known .name }}{{ end -}}
{{- $out := list -}}
{{- range .instance.upstreams -}}
{{- if hasPrefix "rail:" . -}}
{{- $ref := trimPrefix "rail:" . -}}
{{- if not (has $ref $known) -}}
{{- fail (printf "instance %q lists upstream rail:%s, but no instance is named %q" $self $ref $ref) -}}
{{- end -}}
{{- $out = append $out (printf "rail+%s" (include "rail.serviceUrl" (dict "root" $root "name" $ref))) -}}
{{- else -}}
{{- $out = append $out . -}}
{{- end -}}
{{- end -}}
{{- if $out }}{{ join "," $out }}{{ else }}none{{ end -}}
{{- end -}}

{{- define "rail.labels" -}}
app.kubernetes.io/name: rail
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .name }}
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .root.Chart.Name .root.Chart.Version }}
{{- end -}}

{{- define "rail.selector" -}}
app.kubernetes.io/name: rail
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .name }}
{{- end -}}
