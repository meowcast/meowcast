# Sourced by ./meow and cloud/deploy.sh: exports the KEY=VALUE lines of config.env (or
# $MEOW_CONFIG). The file is parsed, never executed, and the environment wins over it.
# Needs $repo (the repository root). Sets $config.
config=${MEOW_CONFIG:-$repo/config.env}
if [ -f "$config" ]; then
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in ''|'#'*) continue ;; esac
        key=${line%%=*}; value=${line#*=}
        case "$key" in *[!A-Z0-9_]*|'') continue ;; esac
        case "$value" in \"*\") value=${value#\"}; value=${value%\"} ;; \'*\') value=${value#\'}; value=${value%\'} ;; esac
        eval "[ -n \"\${$key+x}\" ]" || export "$key=$value"
    done < "$config"
fi
# config_set KEY VALUE: write a value back (used when a deployment learns its own URLs).
config_set() {
    [ -f "$config" ] || cp "$repo/config.env.example" "$config"
    if grep -q "^$1=" "$config"; then
        tmp=$(mktemp); awk -v k="$1" -v v="$2" 'index($0, k"=")==1 {print k"="v; next} {print}' "$config" > "$tmp"
        cat "$tmp" > "$config"; rm -f "$tmp"
    else
        printf '%s=%s\n' "$1" "$2" >> "$config"
    fi
    export "$1=$2"
}
