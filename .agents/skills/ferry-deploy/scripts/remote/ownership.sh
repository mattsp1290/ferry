#!/usr/bin/env bash
h list -A --all --filter "^${RELEASE}\$" -o json
