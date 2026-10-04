#!/usr/bin/env bash
k get namespace "$NAMESPACE" --ignore-not-found -o json
