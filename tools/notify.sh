#!/usr/bin/env bash
# 把一条里程碑推送到 Telegram。
#
# 用法：
#   tools/notify.sh "标题" "正文"
#   echo "正文" | tools/notify.sh "标题"
#   tools/notify.sh --file msg.txt
#
# 为什么要改 HERMES_HOME：
#   本会话的 profile 是 openuuyc，而 openuuyc/.env 里没有 TELEGRAM_BOT_TOKEN
#   （token 只在全局 home 的 .env 里，gateway 就是从那儿跑的）。
#   hermes send 按「home」找凭据，必须指到全局 home。
#
# **必须无条件覆盖，不能写成 ${HERMES_HOME:-默认值}**：
#   gateway 早就把 HERMES_HOME 设成了 profiles/openuuyc，也就是变量处于「已设置」
#   状态，`:-` 的默认值永远不会生效 —— 脚本看着完全正确，实际什么都没做，
#   报错仍然是「Platform 'telegram' is not configured」。这个坑踩过一次，记在这里。
set -euo pipefail

export HERMES_HOME="C:/Users/chaoc/AppData/Local/hermes"
# 顺手摘掉 profile 标记，免得 hermes 仍按 profile 作用域去解析 home。
unset HERMES_SESSION_PROFILE 2>/dev/null || true

# 发一条，并核对回执。「退出码 0」不等于「真发出去了」。
send() {
    local out
    if ! out="$(hermes send "$@" 2>&1)"; then
        echo "推送失败：$out" >&2
        return 1
    fi
    echo "$out"
    if ! grep -q "Sent to" <<<"$out"; then
        echo "警告：命令返回成功，但回执里没有 'Sent to'，请确认是否真的收到。" >&2
        return 1
    fi
}

case "${1:-}" in
    -f|--file)
        send --to telegram --file "${2:?--file 需要路径}"
        ;;
    "")
        echo "用法: tools/notify.sh \"标题\" \"正文\"   或   echo 正文 | tools/notify.sh \"标题\"" >&2
        exit 2
        ;;
    *)
        subject="$1"
        if [ $# -ge 2 ]; then
            body="$2"
        else
            body="$(cat)"
        fi
        send --to telegram --subject "$subject" "$body"
        ;;
esac
