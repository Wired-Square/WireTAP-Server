# shellcheck shell=sh
# Exports the bare KEY=value lines of .env.local, then .env, at the repository
# root, without overriding the environment. Never sourced: .env may be a pipe
# 1Password mounts, so each file is read once.

load_env() {
	for file in "$1/.env.local" "$1/.env"; do
		[ -e "$file" ] || continue
		while IFS= read -r line || [ -n "$line" ]; do
			key=${line%%=*}
			value=${line#*=}
			case $key in "$line" | '' | [0-9]* | *[!A-Za-z0-9_]*) continue ;; esac
			[ -n "$value" ] || continue
			eval "[ -n \"\${$key+x}\" ]" || export "$key=$value"
		done <"$file"
	done
}
