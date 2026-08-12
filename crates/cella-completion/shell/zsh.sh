
# ---- zsh registration ----
#
# Registered as a function via `compdef`, not as a `#compdef`-headed file
# dropped on $fpath. This script is sourced from the end of ~/.zshrc, which in
# every realistic container is *after* compinit has already run: an fpath entry
# added that late is never scanned, and would additionally need .zcompdump
# rebuilt before it took effect. compdef on a live function needs neither.

_cella() {
    local out
    local -a cella_candidates
    out=$(__cella_candidates "${words[CURRENT]}" "${(@)words[1,CURRENT-1]}")
    if [ -n "$out" ]; then
        cella_candidates=("${(@f)out}")
        compadd -- "${cella_candidates[@]}"
    else
        # Nothing to offer: hand the slot to zsh's own default completion so
        # operands still get filenames.
        _default
    fi
}

# Nothing ran compinit (a stripped-down image, or a non-interactive shell
# sourcing the rc file): stay quiet rather than erroring out of the user's rc.
if (( $+functions[compdef] )); then
    compdef _cella cella
fi
