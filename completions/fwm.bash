# Bash 3.2+ completion for fwm. Source this file from ~/.bashrc.
# Candidates come from the local configuration; no daemon is started.

# Remove shell quoting without evaluating substitutions or arbitrary input.
_fwm_unquote() {
    local input=$1 quote= char next i
    REPLY=
    for ((i = 0; i < ${#input}; i++)); do
        char=${input:i:1}
        if [[ $quote == "'" ]]; then
            if [[ $char == "'" ]]; then quote=; else REPLY=$REPLY$char; fi
        elif [[ $char == '\' ]]; then
            next=${input:i+1:1}
            if [[ -z $quote || $next == '$' || $next == '`' || $next == '"' || $next == '\' ]]; then
                ((i++))
                REPLY=$REPLY$next
            else
                REPLY=$REPLY$char
            fi
        elif [[ -n $quote ]]; then
            if [[ $char == '"' ]]; then quote=; else REPLY=$REPLY$char; fi
        elif [[ $char == '"' || $char == "'" ]]; then
            quote=$char
        else
            REPLY=$REPLY$char
        fi
    done
}

_fwm() {
    local LC_ALL=C
    local -a completion_words
    local i j=0 cword=0 join_next=0 token current before length candidate prefix= suffix= readline_word REPLY
    COMPREPLY=()

    # Some Bash versions split '=' and ':' into separate COMP_WORDS entries.
    # Reassemble them so the backend sees the same arguments as Zsh.
    for ((i = 0; i < ${#COMP_WORDS[@]}; i++)); do
        token=${COMP_WORDS[i]}
        if [[ $j -gt 0 && ( $token == '=' || $token == ':' ) ]]; then
            completion_words[j-1]=${completion_words[j-1]}$token
            join_next=1
        elif [[ $join_next == 1 ]]; then
            completion_words[j-1]=${completion_words[j-1]}$token
            join_next=0
        else
            completion_words[j]=$token
            ((j++))
        fi
        [[ $i == "$COMP_CWORD" ]] && cword=$((j - 1))
    done
    [[ $j -gt 0 ]] || return 0

    # COMP_WORDS can include the part of the current word after the cursor.
    # Keep the longest prefix that actually precedes COMP_POINT.
    current=${completion_words[cword]}
    before=${COMP_LINE:0:COMP_POINT}
    for ((length = ${#current}; length > 0; length--)); do
        [[ ${before: -length} == "${current:0:length}" ]] && break
    done
    _fwm_unquote "${current:length}"
    suffix=$REPLY
    completion_words[cword]=${current:0:length}
    for ((i = 0; i < j; i++)); do
        _fwm_unquote "${completion_words[i]}"
        completion_words[i]=$REPLY
    done

    # Readline replaces only the fragment after a word break, even when Bash
    # kept --server=value in one COMP_WORDS entry. Do not duplicate that prefix.
    current=${completion_words[cword]}
    _fwm_unquote "${2-${COMP_WORDS[COMP_CWORD]}}"
    readline_word=$REPLY
    if [[ $current == *"$readline_word" ]]; then
        prefix=${current:0:${#current}-${#readline_word}}
    fi

    while IFS= read -r candidate; do
        [[ -n $candidate && $candidate == "$prefix"* ]] || continue
        # Readline leaves text after the cursor intact. A candidate must share
        # that suffix; return only its prefix so it is not inserted twice.
        if [[ -n $suffix ]]; then
            [[ $candidate == *"$suffix" ]] || continue
            candidate=${candidate:0:${#candidate}-${#suffix}}
        fi
        COMPREPLY[${#COMPREPLY[@]}]=${candidate:${#prefix}}
    done < <("${completion_words[0]}" __complete "$cword" -- "${completion_words[@]}" 2>/dev/null)
}

# filenames makes Readline quote spaces and shell metacharacters safely and
# preserves the trailing slash of directory candidates, including on Bash 3.2.
complete -o filenames -F _fwm fwm
