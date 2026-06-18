#!/usr/bin/env bash
miniserve . \
  --interfaces 127.0.0.1 \
  --index index.html \
  --port 8080 \
  --header "Cache-Control:no-cache" \
  -v