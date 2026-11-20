# mine

Claude Code 계정 두 개와 Codex를 한 터미널 작업방에서 사용합니다. 작업을 선택하면 대화·인수인계 메모를 함께 보고, 사용할 계정과 모델을 정할 수 있습니다. 순정 CLI는 tmux에서 실행합니다.

## 시작

macOS 또는 Linux에서 Python 3.8+, tmux 3.2+, Claude Code, Codex CLI가 필요합니다. 파일 변경 보기에는 GitButler CLI(`but`)를 사용합니다. 저장소에서 `export PATH="$PWD:$PATH"`를 실행한 뒤 작업할 프로젝트의 루트로 이동하세요.

```sh
mine login claude-a
mine login claude-b
mine login codex
mine ui payments
```

두 Claude 계정은 별도의 설정 디렉터리를 쓰며, Codex는 기존 사용자 로그인을 사용합니다. 처음 설정할 때 각 CLI에서 활성 로그인 방법을 확인하세요. 실제 요청은 선택한 계정의 사용량을 소비합니다.

## 한 작업에서 Claude → Codex → Claude

작업 화면에 아래 내용을 한 줄씩 입력합니다. 일반 텍스트는 선택한 실행기로 보내고, 답변은 같은 대화 기록에 저장합니다.

```text
/use claude-a sonnet
취소 처리를 수정하고 변경 이유를 설명해줘.
/recommend 독립 코드 리뷰
/use codex
방금 변경한 취소 처리를 리뷰해줘.
/use claude-a
리뷰에서 지적한 부분을 확인해줘.
```

- `/native`는 선택한 계정의 순정 터미널을 엽니다. tmux 안에서는 팝업에 열립니다. 기본 키 `Ctrl-b d`로 분리하면 작업 화면으로 돌아옵니다. 명령 실행이나 권한 승인이 필요한 경우 순정 화면에서 직접 처리할 수 있습니다.
- `/files`는 현재 프로젝트의 미커밋 diff를 보여줍니다. `/history`는 작업 기록 전체, `/room 이름`은 다른 작업방, `/quit`은 화면 종료입니다.
- `/usage`는 Codex App Server에서 한도를 조회하고 Claude 순정 터미널의 상태줄이 마지막으로 관측한 한도와 함께 표시합니다. Claude 상태줄은 첫 모델 응답 후에 값이 생길 수 있습니다. `/recommend`는 최근 15분 이내 관측된 한도, 직전 응답 계정 및 리뷰 요청 여부를 사용합니다. 추천만 표시하며 배정은 사용자가 합니다.
- SSH에서는 같은 호스트·사용자의 같은 프로젝트 경로에서 실행하세요. 공통 화면의 요청도 SSH를 끊은 뒤 계속 실행하려면 `tmux new -A -s mine-console` 안에서 `mine ui payments`를 실행하세요.

## 헤드리스 명령

```sh
mine rooms
mine usage
mine recommend payments '취소 처리 리뷰'
mine ask payments codex '이전 변경을 리뷰해줘'
mine note payments '배포는 별도 승인 후 진행'
mine history payments
mine open payments claude-b --model sonnet --detach
mine status payments
```

작업 기록은 `~/.mine/rooms`에 저장합니다. 기존 `handoff.jsonl` 메모도 읽습니다. `MINE_STATE_DIR`로 저장 위치와 Claude 프로필 루트를 변경할 수 있습니다.

## 인계 범위

공통 화면은 계정별 CLI 세션 ID를 저장해 다음 요청에 재사용합니다. 계정 전환 시 최근 공유 기록(최대 16,000자)과 전체 기록 경로를 전달합니다. 작업방 이름이 같아도 프로젝트 경로가 다르면 기록이 분리됩니다.

새로 연 순정 세션에서는 `UserPromptSubmit`과 `Stop` 훅이 사용자 질문과 최종 답변을 같은 기록에 추가합니다. **Codex 순정 세션에서 새 훅 검토가 표시되면 `/hooks`에서 해당 작업방의 mine 훅을 확인하세요.** 훅이 꺼져 있거나 구버전으로 열린 세션이면 순정 대화 수집이 작동하지 않습니다. 기존 세션은 작업을 마치고 CLI를 종료한 뒤 다시 열면 새 설정을 받습니다. Claude 순정 세션에서는 mine 사용량 표시줄이 적용됩니다.

공통 화면과 순정 터미널은 대화와 작업 파일을 공유합니다. 실행 중인 도구의 승인이나 중간 출력은 해당 순정 세션에 남습니다. 순정 터미널에서 파일을 편집 중이라면 그 턴이 끝난 뒤 넘기세요. 실패한 요청은 기록에 표시되며 자동 재실행되지 않습니다.

`python3 -B -m unittest discover -s tests -v`로 실제 모델 호출 없이 계정 간 왕복과 실패 처리를 검증할 수 있습니다.
