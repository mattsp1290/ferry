#!/usr/bin/env bash
k get pods -l "app.kubernetes.io/instance=$RELEASE" -o 'jsonpath={range .items[*]}{.metadata.uid}{"\n"}{end}' | sort
