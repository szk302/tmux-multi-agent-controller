# tmac

`tmac`（Tmux Multi-Agent Controller）は、フックを持つ対話型の AI CLI（Claude Code、Codex CLI）を tmux のペインで起動し、CLI のフックから状態を受け取って、**入力してよいときだけ入力する** CLI です。人はいつでもペインを見て割り込めます。人がペインで入力すると、tmac は自動の入力をしばらく止めます。

- 状態（`starting`・`idle`・`busy`・`waiting`・`exited`）は、CLI のフックで知ります。画面の文字は読みません。
- 常駐プロセスはありません。記録は `~/.tmac/agents/<名前>/` の YAML ファイルで、`ls` と `cat` で追えます。
- 入力は、名前付きバッファとブラケットペーストで貼り付け、`UserPromptSubmit` のフックで受け付けられたことを確かめます。確かめられなければ、再送せずに後続の入力を止めます。
- エージェントは、docker compose と同じく、リポジトリ名のプロジェクトごとに分かれます。複数のリポジトリで同じ名前を使っても衝突しません。
- ワークフロー（[Dagu](https://github.com/dagu-org/dagu) など）、ほかの AI、人の手作業のどれからも、同じコマンドで呼べます。メッセージは [lmm](https://github.com/szk302/local-markdown-mail) と組み合わせます。

仕様は [docs/prd.md](docs/prd.md) を参照してください。

## インストール

Rust ツールチェーンは [mise](https://mise.jdx.dev/) で管理しています。

```bash
mise install                 # mise.toml の Rust をインストール
cargo install --locked --path .
```

実行には tmux と `/bin/sh` が必要です（tmux 3.7c で確認）。

## 使い方

```bash
# 起動して、入力を受け付けられるようになるまで待つ（"repo/impl-001 %3" のように出力）
tmac spawn impl-001 --cwd ~/work/repo/.worktree/feat/x --pass-env PATH -- claude
tmux attach -t repo                         # ペインを見る（セッションはプロジェクト、ウィンドウはエージェント名）

tmac send impl-001 "README の誤字を直してください"   # 入力できるまで待って入力する
tmac wait impl-001                          # idle になるまで待つ
tmac status impl-001                        # 状態を YAML で表示する
tmac list                                   # このプロジェクトの一覧（-a ですべてのプロジェクト）

tmac hold impl-001 --note "仕様を確認中"    # 自動の入力を止める（実行中の入力が終わるまで待つ）
tmac release impl-001
tmac kill impl-001                          # 終了して、ペインと記録を削除する
```

### プロジェクト

エージェントは「プロジェクト/名前」で識別します。プロジェクトは docker compose と同じく、次の順で決まります。

1. `-p, --project <PROJECT>`（すべてのコマンドに共通）
2. 環境変数 `TMAC_PROJECT`
3. ディレクトリが属するリポジトリの名前（`spawn` はエージェントの作業ディレクトリ、それ以外はカレントディレクトリで決める）。worktree の中ではメインのリポジトリ名になります。git の外ではディレクトリ名です。英大文字は小文字に、使えない文字は `-` に直します。

```bash
cd ~/work/repo && tmac send impl-001 "..."   # リポジトリの中なら短い名前で指せる
tmac status repo/impl-001                      # どこからでも「プロジェクト/名前」で指せる
tmac -p repo kill impl-001
tmac list -a                                   # すべてのプロジェクト
```

### メッセージ（lmm）と組み合わせる

送信者は、`lmm post` の後に `tmac wake` を実行します。宛先が作業中なら、作業を終えて停止する（`Stop` のフックが動く）のを待ち、何も入力せずに終わります。入力待ちなら、知らせを入力します。

```bash
lmm post -t impl-001 -s "実装依頼" ./request
tmac wake impl-001 "新着メッセージがあります。"
```

停止のたびに新着を渡すには、`Stop` のフックを中継（relay）として設定します。CLI の設定に直接フックを登録すると、tmac は停止が止められたことを知れません。

```bash
tmac spawn impl-001 --relay 'Stop=lmm hook' --env LMM_NAME=impl-001 --pass-env PATH -- claude
```

## ドライバ

| ドライバ | CLI | 注意 |
| :--- | :--- | :--- |
| `claude` | Claude Code | フックは `--settings` で加えます（呼び出し側の `--settings` とは合成します）。`Stop`・`UserPromptSubmit` に tmac 以外のフック（プラグインを含む）があると、起動を断ります。停止を止めないフックなら `--allow-foreign-hooks` で起動できます。 |
| `codex` | Codex CLI | フックは `-c hooks.*` で加えます。Codex は起動しただけではフックを呼ばないため、**最初の指示を起動コマンドの引数で渡します**（例: `-- codex "待機してください"`）。tmac のフックは最初の 1 回だけ、Codex の画面で信頼の確認が要ります。 |

ドライバは、起動コマンドのファイル名（`claude`・`codex`）から決めます。`env` などのラッパーを通す場合は `--driver` を指定します。

## 設定

`$TMAC_HOME/config.yml`（既定 `~/.tmac/config.yml`）。すべて省略できます。

```yaml
tmux:
  session: agents        # spawn がウィンドウを作るセッション（既定: プロジェクトごと）
send:
  settle_sec: 10         # idle がこの秒数続いたら入力する
hold:
  human_sec: 300         # 人の入力を検出した後に止める秒数
relay:
  claude:
    Stop: lmm hook       # 中継（spawn --relay で上書きできる）
```

項目の一覧は仕様の §12 を参照してください。

## 終了コード

| コード | 意味 |
| :--- | :--- |
| 0 | 成功 |
| 1 | 実行時エラー |
| 2 | 使い方・設定の誤り（`spawn` で tmac 以外のフックが見つかった場合を含む） |
| 3 | 時間切れ |
| 4 | エージェントがない（終了した、別の起動に置き換わった場合を含む） |
| 5 | 入力できない、または保留中 |
| 6 | 入力を確認できなかった（`unconfirmed` の保留を設定した） |
| 7 | 中継が入力を拒否した |

`tmac hook`（CLI のフックが呼ぶ）は、中継を実行した場合はその終了コードを返し、それ以外では 2 を返しません。

## 開発

```bash
mise run build
mise run test    # 結合テストは tmux と Python 3.11 以上を使う
mise run lint
```

結合テスト（`tests/agents.rs`）は、テストごとに専用の tmux サーバーとデータディレクトリを作り、フックを本物と同じように呼ぶ偽の CLI（`tests/fake_cli.py`）を起動します。

## ライセンス

[MIT](LICENSE)
