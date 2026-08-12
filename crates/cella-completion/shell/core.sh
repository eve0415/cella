
# ---- shared candidate engine ----
#
# Portable POSIX shell on purpose. The identical text is sourced by bash and by
# zsh, sitting under one generated data section, so the two shells cannot
# disagree about what cella accepts. No arrays, no [[ ]], no `local`.
#
# Contract:
#
#   __cella_candidates <current-word> <word0> [word1 ...]
#
# prints one candidate per line. <word0> is the program name; the rest are the
# words typed before the cursor. Printing nothing means "fall back to whatever
# the shell would have done on its own" — that is how operand slots (branch
# names, paths) reach the shell's default completion.
#
# Every variable is prefixed, because a POSIX function has no scoping and this
# runs inside the user's interactive shell.

__cella_candidates() {
    __cella_cur=$1
    [ $# -gt 0 ] && shift    # the word being completed
    [ $# -gt 0 ] && shift    # the program name

    __cella_cmd=
    __cella_sub=
    __cella_prev=
    __cella_wants_value=0

    while [ $# -gt 0 ]; do
        __cella_word=$1
        shift
        __cella_prev=$__cella_word

        # The previous word claimed this one as its value.
        if [ "$__cella_wants_value" = 1 ]; then
            __cella_wants_value=0
            continue
        fi

        case "$__cella_word" in
            --)
                # Past the separator everything belongs to the user's own
                # command, and we know nothing about it.
                return 0
                ;;
            -*)
                if __cella_takes_value "$__cella_word"; then
                    __cella_wants_value=1
                fi
                continue
                ;;
        esac

        if [ -z "$__cella_cmd" ]; then
            __cella_cmd=$__cella_word
        elif [ -z "$__cella_sub" ]; then
            __cella_sub=$__cella_word
        fi
    done

    # An option is waiting for its value: guessing a flag here would be wrong.
    if [ "$__cella_wants_value" = 1 ]; then
        return 0
    fi

    # bash's COMP_WORDBREAKS splits `--label KEY=VALUE` into `--label`, `KEY`,
    # `=`, `VALUE`, so a bare `=` is a value slot as well.
    if [ "$__cella_prev" = "=" ]; then
        return 0
    fi

    __cella_subs=
    __cella_path=$__cella_cmd
    if [ -n "$__cella_cmd" ]; then
        __cella_subs=$(__cella_subcommands "$__cella_cmd")
        if [ -n "$__cella_subs" ] && [ -n "$__cella_sub" ]; then
            __cella_path="$__cella_cmd $__cella_sub"
        fi
    fi

    # Flags only once the user has typed a dash. Offering them unprompted would
    # fill an operand slot with the wrong thing and, worse, suppress the shell's
    # default completion — `complete -o default` and zsh's `_default` engage
    # only when the candidate list came back empty.
    case "$__cella_cur" in
        -*)
            __cella_flags "$__cella_path"
            return 0
            ;;
    esac

    if [ -z "$__cella_cmd" ]; then
        __cella_commands
        return 0
    fi

    if [ -n "$__cella_subs" ] && [ -z "$__cella_sub" ]; then
        printf '%s\n' "$__cella_subs"
    fi

    return 0
}
