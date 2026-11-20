# Turnloom

한 프로젝트에서 Claude Code 계정 두 개와 Codex를 순정 터미널로 열고, 작업방별 메모로 인수인계하는 개인용 CLI입니다.

## 시작하기

macOS 또는 Linux에 Python 3.8+, tmux, Claude Code, Codex CLI가 필요합니다. 저장소를 복제한 뒤 그 디렉터리에서 `export PATH="$PWD:$PATH"`를 실행하면 현재 셸에서 `turnloom`을 사용할 수 있습니다. 프로젝트 **루트 디렉터리**에서 실행하세요.

```sh
turnloom open payments claude-a
```

Claude A와 B는 처음 열 때 각각의 설정 디렉터리로 Claude Code 로그인을 진행합니다. Codex는 현재 사용자의 일반 Codex 로그인을 사용합니다. 구독으로 사용할 때는 셸에 다른 인증 방식이 설정되어 있으면 각 CLI의 로그인 상태를 먼저 확인하세요. 새 세션은 인수인계 메모를 읽도록 첫 프롬프트를 전달하므로 로그인 후 모델 사용량이 발생할 수 있습니다.

tmux에서 Ctrl-b, d를 눌러 분리하고 같은 프로젝트 디렉터리에서 이어서 실행하세요.

```sh
turnloom note payments '취소 처리 수정 완료. 테스트는 아직 실행하지 않음. 다음 작업은 취소 테스트.'
turnloom open payments codex
turnloom history payments
turnloom status payments
turnloom open payments claude-b --model sonnet
```

이미 열려 있는 세션은 원래 대화로 돌아갑니다. 돌아온 세션에서는 “인수인계 메모를 다시 읽고 이어서 진행해”라고 입력하세요. 원격에서는 같은 호스트와 사용자로 SSH 접속하고 같은 프로젝트 루트에서 같은 명령을 실행합니다. 로컬 상태는 기본적으로 `~/.turnloom`에 저장됩니다.

## 첫 버전의 범위

공유되는 것은 사용자가 남긴 메모와 같은 작업 디렉터리의 파일입니다. 터미널을 분리해도 실행 중인 작업은 계속되므로 편집을 넘길 때는 이전 작업이 끝났는지 확인하세요. 전체 대화 타임라인과 자동 메모 추출, 사용 한도 기반 배정, 통합 채팅 UI는 후속 단계입니다.
