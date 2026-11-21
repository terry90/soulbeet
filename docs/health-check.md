# Health check

This page describes `/healthz`, the address Docker and Kubernetes can check to tell whether Soulbeet is up.

`GET /healthz` answers `200 ok` while Soulbeet serves requests and its database answers a query within 2 seconds, and `503 unavailable` otherwise. It needs no sign-in and checks neither slskd nor Navidrome. Use the port you set in `PORT` if it is not `9765`.

## Docker Compose

The image has no shell, `curl` or `wget`, so a Docker health check makes the request with the image's Python, which exits with an error on a `503` or no answer. Add this to the `soulbeet` service of the [Quick Start](../README.md#quick-start):

```yaml
    healthcheck:
      test: ["CMD", "/usr/bin/python3", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:9765/healthz', timeout=5)"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 1m
```

## Kubernetes

Probe it over HTTP:

```yaml
startupProbe:
  httpGet:
    path: /healthz
    port: 9765
  periodSeconds: 5
  failureThreshold: 60 # up to 5 minutes for the first start
livenessProbe:
  httpGet:
    path: /healthz
    port: 9765
  periodSeconds: 30
  timeoutSeconds: 5
  failureThreshold: 3
readinessProbe:
  httpGet:
    path: /healthz
    port: 9765
  periodSeconds: 10
  timeoutSeconds: 5
```
