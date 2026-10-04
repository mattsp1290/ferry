# Extract the literal ferry.toml block from a rendered ConfigMap.
extract_toml() {
    awk '
        /^  ferry.toml: \|/ { on = 1; next }
        on && /^    / { sub(/^    /, ""); print; next }
        on && /^[[:space:]]*$/ { print ""; next }
        on { exit }
    ' "$1"
}
