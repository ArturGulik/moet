#!/usr/bin/env bash
#  __  __            _
# |  \/  | ___   ___| |_
# | |\/| |/ _ \ / _ \ __|
# | |  | | (_) |  __/ |_
# |_|  |_|\___/ \___|\__|

source "$HOME/.bashrc"
export HISTCONTROL=ignorespace
ORIGINAL_PROMPT_COMMAND="$PROMPT_COMMAND"
PREVIOUS_DIR="$PWD"

# Using the path from env, which should be set before
# bash is run inside moet.
TMP_PATH=$MOET_TMP_PATH

if [[ -z "$TMP_PATH" ]]; then
  echo -e "\033[0;31m[MOET] Environment configuration error\033[0m"
  echo "MOET_TMP_PATH variable is not set. If you think this is a bug, please report it here: https://github.com/ArturGulik/moet/issues"
  echo "This variable should be set by moet before running bash. It is needed to store temporary files during runtime."
  echo "Moet will exit now."
  read -p "Press any key..." -n1 -s
  exit 1
fi

function moet_prompt_command {
  \eval "$ORIGINAL_PROMPT_COMMAND"
  if [[ "$PWD" != "$PREVIOUS_DIR" ]]; then
    PREVIOUS_DIR="$PWD"
    \echo "$PWD" > "$TMP_PATH/tmp_pwd"
    \cp "$TMP_PATH/tmp_pwd" "$TMP_PATH/pwd"
  fi
}

PROMPT_COMMAND='moet_prompt_command'
