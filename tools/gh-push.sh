#!/usr/bin/env bash
# 把当前提交推送到 GitHub。
#
# 为什么需要这个脚本，而不是直接 `git push`：
#   本机没装 gh CLI，凭据只能靠设备码流程临时取。而把令牌写进
#   `.git/config` 的 remote URL、或写进 `~/.git-credentials` + 全局
#   credential.helper，都是**明文留痕**且改动了机器/仓库配置。
#   这里改成：令牌单独放一个 0600 文件，只在推送这一瞬间注入 URL，
#   `.git/config` 里始终保持不含凭据的干净 remote。
#
# 令牌文件位置：$HOME/.config/uulan/gh-token（可用 UULAN_GH_TOKEN_FILE 覆盖）
# 首次获取令牌（设备码流程，见 docs/ 或 skills/github/github-auth）：
#   取码 → 用户在 https://github.com/login/device 输入 → 轮询把 access_token 写入该文件
#
# 用法：
#   bash tools/gh-push.sh                  # 推当前分支到 main
#   bash tools/gh-push.sh --tags           # 额外推标签
set -euo pipefail

SLUG="${UULAN_GH_SLUG:-zncznc/uulan}"
USER_NAME="${SLUG%%/*}"
BRANCH="${UULAN_GH_BRANCH:-main}"
TOKEN_FILE="${UULAN_GH_TOKEN_FILE:-$HOME/.config/uulan/gh-token}"

clean_url="https://github.com/${SLUG}.git"

if [ ! -s "$TOKEN_FILE" ]; then
    cat >&2 <<EOF
没有找到 GitHub 令牌：$TOKEN_FILE

需要先走一次设备码授权（本机没有 gh CLI，只能这样）：
  1. 取码并让用户在 https://github.com/login/device 输入
  2. 轮询拿到 access_token 后写入：$TOKEN_FILE（权限 0600）

授权一次之后，这个脚本就能一直非交互地推送。
EOF
    exit 1
fi

token="$(tr -d '\r\n' < "$TOKEN_FILE")"

# 保证 origin 是**不含凭据**的干净地址，方便平时 git status/log 用
if ! git remote get-url origin >/dev/null 2>&1; then
    git remote add origin "$clean_url"
elif [ "$(git remote get-url origin)" != "$clean_url" ]; then
    git remote set-url origin "$clean_url"
fi

echo "推送 $(git rev-parse --short HEAD) → ${SLUG} 的 ${BRANCH} 分支"

# git push 会把带凭据的 URL 原样打到 stderr，所以必须过一遍 sed 把令牌抹掉。
set +e
out="$(git push "https://${USER_NAME}:${token}@github.com/${SLUG}.git" "HEAD:${BRANCH}" 2>&1)"
rc=$?
set -e
printf '%s\n' "$out" | sed "s|${token}|***|g"

if [ "$rc" -ne 0 ]; then
    echo "推送失败（退出码 $rc）" >&2
    exit "$rc"
fi

if [ "${1:-}" = "--tags" ]; then
    set +e
    tout="$(git push "https://${USER_NAME}:${token}@github.com/${SLUG}.git" --tags 2>&1)"
    trc=$?
    set -e
    printf '%s\n' "$tout" | sed "s|${token}|***|g"
    [ "$trc" -ne 0 ] && { echo "标签推送失败（退出码 $trc）" >&2; exit "$trc"; }
fi

echo "完成：${clean_url}"