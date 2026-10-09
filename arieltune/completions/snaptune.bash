# bash completion for `snaptune` — BC-250 kernel snapshots
#
# Install system-wide:
#   sudo install -m 0644 completions/snaptune.bash /etc/bash_completion.d/snaptune
#   # start a new shell (or: source /etc/bash_completion.d/snaptune)
#
# Per-user alternative:
#   install -m 0644 completions/snaptune.bash ~/.bash_completion.d/snaptune
#   echo 'source ~/.bash_completion.d/snaptune' >> ~/.bashrc
#
# Usage:
#   snaptune <TAB>              # verbs
#   snaptune show <TAB>         # snapshot ids
#   snaptune show snap-a<TAB>   # only the ids starting with snap-a
#   snaptune list --al<TAB>     # flags, too
#   arieltune snapshot <TAB>    # the same, through the suite's multi-call name
#   at snap <TAB>               # and through the short alias
#   snapshot show snap-a<TAB>   # the retired shell tool's name, now a symlink here
#
# There is deliberately no verb list in this file. The tool answers `__complete` for
# the words already typed and prints the candidates, so a verb or an alias added to
# the CLI shows up here without this file changing, and one removed from the CLI
# cannot go on being offered. A second copy of a program's own verbs is a second
# thing to forget, and a tab stop that quietly stops offering something is noticed
# only by whoever was relying on it.
_snaptune_complete() {
    local cur="${COMP_WORDS[COMP_CWORD]}"
    local prog="${COMP_WORDS[0]##*/}"
    local -a cmd=("${COMP_WORDS[0]}") rest=()
    local skip=1

    # `arieltune snapshot …` / `at snap …`: for the suite's multi-call names the
    # namespace is part of the command. Every other namespace is not ours to speak
    # for — return nothing and let the shell's default completion take over.
    if [ "$prog" != "snaptune" ] && [ "$prog" != "snapshot" ]; then
        case "${COMP_WORDS[1]:-}" in
        snapshot | snap)
            cmd+=("${COMP_WORDS[1]}")
            skip=2
            ;;
        *) return 0 ;;
        esac
    fi

    # Everything typed before the word being completed, minus the namespace. The
    # partial word itself is left out on purpose: the tool answers for the position
    # and `compgen -W` filters by what has been typed, which is what makes
    # `snap-a<TAB>` cycle just the ids that match.
    if [ "${COMP_CWORD:-0}" -gt "$skip" ]; then
        rest=("${COMP_WORDS[@]:$skip:$((COMP_CWORD - skip))}")
    fi

    # A failure here (no store, tool not installed) leaves the candidates empty,
    # which reads as "nothing to suggest" — never an error mid-command.
    local candidates
    candidates=$("${cmd[@]}" __complete "${rest[@]}" 2>/dev/null) || return 0
    COMPREPLY=($(compgen -W "${candidates}" -- "${cur}"))
    return 0
}

# `snapshot` is registered now that the bc-250-snapshotter shell tool is retired and
# the name is a symlink to this binary (docs/snapshot-tool.md §3.5). Installing this
# file replaces the shell tool's own /etc/bash_completion.d/snapshot, which completed
# by calling `snapshot list --ids`.
complete -o default -F _snaptune_complete snaptune snapshot
# The suite's multi-call names: this file speaks for the snapshot namespace only.
complete -o default -F _snaptune_complete arieltune at
