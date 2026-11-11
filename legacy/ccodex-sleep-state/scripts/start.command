#!/bin/sh
# Locate the executable relative to this file, not Finder's working directory.
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
app="$script_dir/ccodex-sleep-state"
if [ ! -x "$app" ]; then app="$script_dir/../ccodex-sleep-state"; fi
if [ ! -x "$app" ]; then
  printf '%s\n' '未找到可执行的 ccodex-sleep-state。请完整解压发布包；不要关闭系统安全检查。' >&2
  exit 1
fi
printf '%s\n' '正在启动。请保持这个窗口打开；退出时按 Ctrl+C，以便恢复 Codex 配置。'
"$app" setup "$@"
result=$?
if [ "$result" -ne 0 ]; then
  printf '%s\n' '启动未完成，请保留上方错误信息。不要删除配置或备份。' >&2
  if [ -t 0 ]; then printf '%s' '按回车关闭窗口…'; read -r answer; fi
fi
exit "$result"
