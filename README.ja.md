# ytm-tui

[English](README.md) · [한국어](README.ko.md) · **日本語**

### [▶ ライブデモ・機能ツアー → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

ターミナルの中で楽しむ YouTube Music。速くて、キーボードで操れて、RAM をじわじわ食うブラウザのタブも広告もありません。DJ Gem ストリーミング、本物のアルバムアート、リモート操作まで — すべて3文字のコマンド一つで: `ytt`。

Rust + ratatui。MIT。

---

## インストール

各コマンドは `ytt` と補助ツール（mpv、yt-dlp、ffmpeg）を**一度に**まとめて入れます。

| OS | 一行でOK |
| --- | --- |
| **macOS** | `brew install Ochichan/tap/ytm-tui` |
| **Windows** | `scoop bucket add extras; scoop bucket add ytm-tui https://github.com/Ochichan/scoop-bucket; scoop install ytm-tui` |
| **Linux** — 任意のディストロ、[Nix](https://nixos.org/download) | `nix run github:Ochichan/ytm-tui` |
| **Linux** — Arch | `yay -S ytm-tui-bin` |
| **Linux** — その他 | 下のインストーラを実行 |
| **ソースからビルド** | `./install.sh --build`（[Rust](https://rustup.rs) が必要） |

```sh
curl -fsSL https://raw.githubusercontent.com/Ochichan/ytm-tui/main/install.sh | bash
```

> `curl | bash` とソースビルドは `ytt` **だけ**を入れます。補助ツールは自分で入れるか（`brew install mpv yt-dlp ffmpeg`、`sudo apt install mpv yt-dlp ffmpeg`、`sudo pacman -S mpv yt-dlp ffmpeg`）— インストール後に `ytt doctor` で何が足りないか確認してください。
> Windows では、Scoop が `ytt-tray.exe` と **YtmTui Tray** ショートカットも入れます。これは通知領域のヘルパーです。ターミナルで動く `ytt` セッションのタスクバーボタンは、引き続き Windows Terminal のものです。
> Windows の tray 自動起動は任意です。有効化は `ytt-tray --install-startup`、削除は `ytt-tray --uninstall-startup` を実行してください。
> バックグラウンド再生: `ytt daemon start --resume` で保存済みキューを headless 音楽デーモンとして開始し、`ytt -r status`、`ytt -r pp`、`ytt -r next`、`ytt -r play "lofi"`、`ytt daemon stop` で操作できます。

---

## クイックスタート

```sh
ytt
```

1. **`s`** を押して、曲名を入力し **`Enter`**。
2. **`↑`/`↓`** で移動、**`Enter`** で再生。
3. いつでも **`?`** を押せば全キー一覧（常に最新）。

これだけ。音楽が流れます。

> **何かおかしい？** **`ytt doctor`** を実行すれば mpv、yt-dlp、ffmpeg を点検して、何を直すべきか正確に教えてくれます。`ytt: command not found` が出たら、ターミナルを開き直して `PATH` を反映させてください。

---

## できること

- **DJ Gem ストリーミング** — **`Ctrl+R`** を押せば、今聴いている曲を中心に途切れないラジオが始まります。雰囲気は3つから: Focused、Balanced、Discovery。**`w`** を押せば、各曲を選んだ理由をやさしい言葉で見せてくれます。
- **本物のアルバムアート** — 対応するターミナルなら、プレイヤーに実際のカバー画像を描きます。その下には時間同期した歌詞が流れます（**`Shift+L`**）。
- **リモート操作 + デーモンモード** — 実行中の TUI や headless デーモンを別ターミナルから操作: `ytt -r pp`、`ytt -r next`、`ytt -r status`、`ytt -r play "city pop"`。
- **検索 · ライブラリ · キュー** — **`s`** で検索、**`l`** でライブラリ（お気に入り・履歴・ダウンロード）、**`c`** でキュー。
- **自分好みに** — テーマ11種、すべての色を hex で調整、すべてのキーを再設定、10バンド EQ、そして静かな静止画面からくるくる回る ASCII ドーナツまでのアニメーション。
- **DJ Gem アシスタント** *(任意)* — **`g`** を押して言葉で頼むだけ: *「ローファイをかけて」「アップテンポな曲を3つキューに入れて」*。無料の Google Gemini キーが必要ですが、それ以外の機能はキーなしでも全部動きます。
- **ダウンロード** — **`d`** で曲を保存してオフライン再生。

アプリのインターフェースは**英語と한국어（韓国語）**に対応しています（設定 → 一般 → 言語）。この README は [English](README.md)、[한국어](README.ko.md) でも読めます。

---

## 主要なキー

アプリ内で **`?`** を押せば全チートシートが出て、*あなたが変えたキー*がそのまま反映されます。下記のキーはすべて再設定可能です（設定 → ショートカット）。基本はこれだけ:

| キー | 機能 |
| --- | --- |
| `Space` | 再生 / 一時停止 |
| `←` / `→` | 後ろ / 前へシーク |
| `↑` / `↓` | 音量アップ / ダウン |
| `n` / `p` | 次 / 前の曲 |
| `s` | 検索 |
| `Shift+S` | シャッフル |
| `l` | ライブラリ |
| `a` | ライブラリタブ全体を再生 |
| `c` | キュー |
| `f` | お気に入り / 評価 |
| `d` | ダウンロード |
| `Ctrl+R` | DJ Gem ストリーミングの切り替え |
| `Shift+L` | 歌詞 |
| `g` | DJ Gem アシスタント |
| `w` | DJ Gem がこの曲を選んだ理由 |
| `,` | 設定 |
| `?` | 全キー一覧 |
| `Ctrl+Q` | 終了 |

> **韓国語キーボード?** ショートカットは2ボル式の字母を理解するので、`ㅂ` は `q`、`ㄱ` は `r` のように動きます — 入力切り替えは不要です。

---

## リモート操作

`ytt` が再生中なら、別のターミナル — またはメディアキー — から `ytt -r` で操作できます:

```sh
ytt -r pp          # 再生 / 一時停止
ytt -r next        # 次の曲
ytt -r streaming on    # ストリーミングをオン
ytt -r play "lofi"      # デーモン: 検索して最初の結果を再生
ytt -r enqueue "city pop"  # デーモン: 検索して最初の結果をキューに追加
ytt -r status      # 一行の「再生中」
ytt -r quit        # 止めて終了
```

メディアキーに割り当て（i3 / sway）:

```
bindsym XF86AudioPlay exec ytt -r pp
bindsym XF86AudioNext exec ytt -r next
```

ターミナルなしで再生を続けるなら、デーモンを起動します:

```sh
ytt daemon start --resume   # 保存済みキュー/セッションを復元して再生
ytt daemon status --json    # スクリプト向け owner/status スナップショット
ytt daemon stop             # デーモン停止と mpv の後始末
```

デーモンの resume は、保存済みのキュー順、カーソル、シャッフル/リピート、通常/ラジオモードのキューを復元します。自動ストリーミングも TUI と同じ推薦経路で headless のままキューを補充します。`ytt -r play …` と `ytt -r enqueue …` はデーモン検索コマンドです。standalone TUI が所有者の場合は `daemon_required` で拒否されます。

`ytt` を二度起動しても、スピーカーを取り合う二つ目のプレイヤーは生まれず、すでに動いている方への呼びかけ方を教えてくれるだけです。（本当に二つ欲しいなら `ytt --new-instance`。）全コマンドは `ytt -r --help` と `ytt daemon --help`。

---

## トラブルシューティング

| 症状 | 対処 |
| --- | --- |
| 再生されない / 再生した瞬間にエラー | mpv または yt-dlp がない、または `PATH` にありません。`ytt doctor` を実行。 |
| `ytt: command not found` | ターミナルを開き直してください。それでもダメなら、インストール先が `PATH` にないので、インストーラが追加すべき行を表示します。 |
| 昨日は動いたのに今日はダメ | YouTube が何か変えました — yt-dlp を更新（`brew upgrade yt-dlp`、`scoop update yt-dlp`、またはパッケージマネージャ）。 |
| 特定の曲だけ再生できない | サインインが必要な場合があります — 下の Cookie を参照。 |
| アルバムアートが出ない | 既定でオフ、ターミナル依存です。**アルバムアート**（設定 → 一般）をオンにして再起動。 |
| DJ Gem が応答しない | 設定 → DJ Gem で無料の Gemini キーを入れ、**DJ Gem を有効化**をオンに。 |
| キーを変えて大混乱 | 設定 → 一般 → **ショートカットを初期化**。 |

それでも詰まったら [Issue を立てて](https://github.com/Ochichan/ytm-tui/issues) OS を書き添えてください。

---

## サインイン & ファイルの場所

**Cookie（任意）。** ほとんどの場合は不要です — 公開されている曲は匿名でも検索・再生できます。メンバー限定や地域制限の曲にアクセスするには、YouTube Music の Cookie を **Netscape 形式**で `cookies.txt` に書き出し（macOS: `~/Music/ytm-tui/cookies.txt`、Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`）、`ytt` を再起動してください。**このファイルはパスワードと同じように扱ってください。** 設定 → 一般 で別のパスを指定することもできます。

**設定 & ダウンロード。**
- 設定ファイル: `~/Library/Application Support/ytm-tui/config.json`（macOS）· `~/.config/ytm-tui/config.json`（Linux）· `%APPDATA%\ytm-tui\config.json`（Windows）。
- ダウンロードの既定は `~/Music/ytm-tui`。**ダウンロード先**設定か `YTM_DOWNLOAD_DIR` で変更できます。
- 環境変数 `GEMINI_API_KEY` と `YTM_DOWNLOAD_DIR` は、起動時に保存済みの設定より優先されます。

---

## 特別な感謝

🙏 **[@ZZNN75](https://github.com/ZZNN75)** さんに大きな感謝を — すみずみまでつついて、わざと壊して、本物の QA 時間を費やしてくれました。あなたが*ぶつからずに済む*ざらついた部分が滑らかなのは、先に彼らがぶつかって声を上げてくれたからです。🫡

## ライセンス

MIT。フォークするも、配布するも、ご自由に。
