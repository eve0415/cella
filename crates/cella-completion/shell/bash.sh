
# ---- bash registration ----
#
# Deliberately does not use bash-completion's `_init_completion` /
# `_get_comp_words_by_ref`: plenty of container base images never install that
# package, and depending on it would leave this silently dead in exactly the
# minimal images cella targets. COMP_WORDS/COMP_CWORD are bash builtins and are
# always there.
#
# `-o bashdefault -o default` is what lets an empty candidate list fall through
# to filenames, so operand slots still complete usefully.

_cella() {
    local cur=${COMP_WORDS[COMP_CWORD]}
    local IFS=$'\n'
    COMPREPLY=($(compgen -W "$(__cella_candidates "$cur" "${COMP_WORDS[@]:0:COMP_CWORD}")" -- "$cur"))
}

complete -o bashdefault -o default -F _cella cella
