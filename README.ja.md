# ytm-tui

[English](README.md) · [한국어](README.ko.md) · **日本語**

[![Release](https://img.shields.io/github/v/release/Ochichan/ytm-tui)](https://github.com/Ochichan/ytm-tui/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-8aadf4.svg)](LICENSE)

### [▶ ライブデモ・機能ツアー → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

ターミナルの中で楽しむ YouTube Music。速くて、キーボードで操れて、RAM をじわじわ食うブラウザのタブも広告もありません。
DJ Gem ストリーミング、同期歌詞つきの本物のアルバムアート、Last.fm / ListenBrainz スクロブル、コマンド一行で終わる
Spotify のお引っ越し、どこからでも効くリモート操作まで — すべて3文字のコマンド一つで: `ytt`。

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
> Windows では Scoop が `ytt-tray.exe` と **YtmTui Tray** ショートカットも入れます — 通知領域のミニプレイヤーです（[詳細は下](#メディアキー--os-統合)）。ターミナルで動いている `ytt` セッションのタスクバーボタンは、これまでどおり Windows Terminal のものです。
> macOS でも Homebrew とリリースアーカイブに `ytt-tray` が同梱されます — 同じ相棒がメニューバーに住み着きます（v1.5.8 より後のリリース）。
> トレイのスタートアップ登録はどちらも任意です。有効化は `ytt-tray --install-startup`、解除は `ytt-tray --uninstall-startup`。
> バックグラウンド再生: `ytt daemon start --resume` で保存済みキューから headless 音楽デーモンを起動し、`ytt -r status`、`ytt -r pp`、`ytt -r next`、`ytt -r play "lofi"`、`ytt daemon stop` で操作します。

---

## クイックスタート

```sh
ytt
```

1. **`s`** を押して、曲名を入力し、**`Enter`**。
2. **`↑`/`↓`** で移動して **`Enter`** で再生。
3. いつでも **`?`** を押せば、常に最新の全キー一覧が出ます。

以上。音楽が流れます。

> **何かおかしい？** **`ytt doctor`** を実行してください — mpv、yt-dlp、ffmpeg を点検して、直すべき箇所を正確に教えてくれます。`ytt: command not found` が出る？ 新しいターミナルを開いて `PATH` を追いつかせましょう。

---

## できること

- **DJ Gem ストリーミング** — **`Ctrl+R`** で、今聴いている曲を軸にした果てしないステーションを作ります。ムードは3つ: Focused、Balanced、Discovery。**`w`** を押すと、それぞれの曲を選んだ理由をやさしい言葉で見せてくれます。
- **カタログは六つ、検索窓は一つ** — ホームは YouTube Music ですが、検索で **`Tab`** を押すと SoundCloud、Audius、Jamendo、Internet Archive、Radio Browser に切り替わります（全部まとめても可、結果には `[SRC]` タグ）。アプリ全体をネットラジオのチューナーに変える専用ラジオモード（**`Alt+Shift+R`**）もあります — ラジオ専用のお気に入りと履歴つき。
- **本物のアルバムアート + 同期歌詞 + MV** — 実際のカバー画像がプレイヤーにそのまま描かれます（Kitty/Sixel/iTerm2 グラフィック自動検出）。その下を時間同期の歌詞が流れ（**`Shift+L`**）、聴くだけじゃ物足りなければ **`v`** — ミュージックビデオがターミナルの上の小さな mpv ウィンドウに浮かびます。
- **検索 · ライブラリ · キュー · プレイリスト** — **`s`** で検索、**`l`** でライブラリ（お気に入り・履歴・ダウンロード・プレイリスト）、**`c`** でキュー。アプリ内でプレイリストを作るか（**`n`**）、DJ Gem に作ってもらいましょう。
- **ダウンロード** — **`d`** を押すとカバーアートとタグ入りの m4a で保存され、ダウンロードタブからオフライン再生できます。
- **スクロブル** — Last.fm と ListenBrainz。クラッシュに強いオフラインキューと、いいね→love 同期つき。[詳細は下](#スクロブル-lastfm--listenbrainz)。
- **Spotify インポート / エクスポート** — いいねもプレイリストも、チェックポイント・再開つきのコマンド一行で引っ越せます。[詳細は下](#spotify-インポート--エクスポート)。
- **どこからでも操作** — メディアキー、macOS コントロールセンター、Windows SMTC + トレイ・ミニプレイヤー、Linux MPRIS、どのシェルからでも `ytt -r`、そしてターミナルすら不要な headless デーモン。[詳細は下](#メディアキー--os-統合)。
- **自分好みに** — テーマ13種、34の色ロールすべてを hex で編集、全キー再設定、プリセット付き10バンド EQ、静かな静止画面からくるくる回る ASCII ドーナツまでのアニメーション — さらに素の Linux コンソールでも動くレトロモード（ASCII アルバムアート付き）。
- **DJ Gem アシスタント** *（任意）* — **`g`** を押して言葉で頼むだけ: *「lo-fi をかけて」「アップテンポを3曲キューに」「雨の日プレイリストを作って」*。無料の Google Gemini キーが必要で、それ以外の機能はキーなしで全部動きます。

アプリの UI は **English と 한국어** に対応しています（設定 → 一般 → 言語）。この README は [English](README.md) と [한국어](README.ko.md) でも読めます。

---

## 基本のキー

アプリで **`?`** を押すと、完全なライブチートシートが出ます — *あなたが変えた*キーがそのまま反映され、下のキーはすべて再設定できます（設定 → ホットキー）。基本だけ:

| キー | 動作 |
| --- | --- |
| `Space` | 再生 / 一時停止 |
| `←` / `→` | 後ろ / 前へシーク |
| `↑` / `↓` | 音量アップ / ダウン |
| `n` / `p` | 次 / 前の曲 |
| `s` | 検索（`Tab` でソース選択） |
| `l` | ライブラリ · `a` タブ全体を再生 · `\` キューへ追加 · `/` フィルタ |
| `c` | キュー |
| `f` | お気に入り / 評価 |
| `d` | ダウンロード |
| `P` | プレイリストへ追加（リスト上では `p`） |
| `Shift+L` | 同期歌詞 |
| `v` | MV オーバーレイ（`V` で位置切り替え） |
| `y` | 曲のリンクをコピー |
| `Ctrl+R` | DJ Gem ストリーミングのオン/オフ |
| `w` | DJ Gem の選曲理由 |
| `g` | DJ Gem アシスタント |
| `Shift+S` / `r` | シャッフル / リピート切り替え |
| `e` | EQ プリセット（Flat · Bass · Treble · Vocal · Rock · Jazz） |
| `<` / `>` | 再生速度（0.5×–2×） |
| `,` | 設定 |
| `Ctrl+Q` | 終了 |

> **ハングル配列でも大丈夫。** ショートカットは 2ボル式の字母を理解するので、`ㅂ` は `q`、`ㄱ` は `r` として効きます — IME を切り替える必要はありません。マウス派なら画面のすべてがクリックでき、ホイールは音量に効きます。

---

## プレイリスト

ライブラリの**プレイリスト**タブには、自分だけのローカルプレイリストが入ります — アプリ内で作り（**`n`**）、どこからでも足し（再生中の曲は **`P`**、リストの行では **`p`**）、好きに整理し、丸ごと再生・キュー投入（**`a`** / **`\`**）。設定ファイルの隣の素朴な `playlists.json` に保存されるので、バックアップはファイルコピーだけです。

Spotify のインポートをここに受けることもでき（`--to local`）、DJ Gem アシスタントが頼まれて作って埋めることもでき、`ytt transfer export local:<名前> --to spotify` で逆方向にも送れます。

---

## リモート操作 & デーモン

`ytt` が再生中なら、別のターミナルから — あるいはメディアキーで — `ytt -r` で操作できます:

```sh
ytt -r pp                  # 再生 / 一時停止   (別名: toggle, play, pause)
ytt -r next / prev         # 曲の移動
ytt -r volume 40           # 音量を直接指定; up / down も可
ytt -r back / fwd          # 設定した間隔でシーク
ytt -r seek-to 90          # 1:30 へジャンプ
ytt -r streaming on        # 無限ストリーミング: on / off / toggle
ytt -r play "lofi"         # デーモン: 検索して最初の結果を再生
ytt -r enqueue "city pop"  # デーモン: 検索して最初の結果をキューへ
ytt -r status              # 一行の「再生中」(--json はスクリプト用、-q は静かに)
ytt -r quit                # 停止して終了
```

メディアキーへの割り当て（i3 / sway）:

```
bindsym XF86AudioPlay exec ytt -r pp
bindsym XF86AudioNext exec ytt -r next
```

ターミナルなしの再生はデーモンで:

```sh
ytt daemon start --resume   # 保存済みキュー/セッションを復元して再生
ytt daemon status --json    # 所有者/状態スナップショット（スクリプト用）
ytt daemon stop             # デーモン停止 + mpv の後始末
```

デーモンの resume は、保存されたキューの順序、カーソル、シャッフル/リピート、通常/ラジオモードのキューを復元します。自動ストリーミングは TUI と同じ推薦経路で headless のままキューを満たし続け、スクロブルも OS メディアコントロールもそのまま動きます。`ytt -r play …` / `ytt -r enqueue …` はデーモン専用の検索コマンドで、単体 TUI は拒否します。

`ytt` を二度起動しても、スピーカーを取り合う二つ目のプレイヤーは生まれません — 今あるものの操作方法を教えてくれるだけです（本当に二つ欲しければ `ytt --new-instance`）。全コマンドは `ytt -r --help` と `ytt daemon --help` で。

---

## メディアキー & OS 統合

`ytt` は OS が音楽を表示するすべての場所に現れます — 初期設定でオン、設定 → 再生 → *OS メディアコントロール* で切れます。TUI でもデーモンでも同じように動きます。

- **macOS** — コントロールセンターの本物の Now Playing カード: アートワーク、ちゃんと動くシークバー、次へ/前へ、Like ボタンまで。AirPods の軸をつまむ操作も期待どおりに効きます。さらに **YtmTui Tray** の相棒がメニューバーに（`ytt-tray`、brew とリリースアーカイブに同梱）— Windows と同じミニプレイヤーとメニューが、時計の隣のワンクリックに。
- **Windows** — アートワークとシーク付きの SMTC メディアオーバーレイ、そして任意の **YtmTui Tray** 相棒（Scoop が入れます）: 左クリックで Now / Queue / Stream / Tune タブのミニプレイヤー、右クリックでフルメニュー — デーモンの起動/停止、前回セッションの再開、TUI を開く。ログイン時の自動起動は `ytt-tray --install-startup`（任意）。
- **Linux** — 堂々たる MPRIS プレイヤー（`org.mpris.MediaPlayer2.ytmtui`）: playerctl、GNOME/KDE のメディアウィジェット、waybar が全部そのまま認識します。

---

## スクロブル (Last.fm / ListenBrainz)

`ytt` は実際に聴いたものだけをスクロブルします — Now Playing の更新、標準のハーフトラック/4分ルール、
アプリ内いいねの love/unlove 同期、そして丈夫なオフラインキュー（機内で聴いた分はオンラインに戻ると
配達されます — ネットワークを試す*前に*ディスクへ書かれるので、クラッシュしても失いません）。
TUI と headless デーモンの両方で、OS メディアコントロール設定とは独立に動きます。

- **Last.fm** — 設定 → **アカウント** → *Last.fm account* → ブラウザで承認。または
  headless で: `ytt auth lastfm`。埋め込み API 資格情報のない自前ビルドは、
  `config.json` の `scrobble.lastfm.api_key` / `api_secret` で自分のものを設定できます
  （[API アカウントの作成](https://www.last.fm/api/account/create)）。
- **ListenBrainz** — [ユーザートークン](https://listenbrainz.org/settings/)を
  設定 → アカウントに貼るか、`ytt auth listenbrainz <token>` を実行。セルフホストは
  `scrobble.listenbrainz.api_url` を設定してください。
- サービスごとのトグル、いいね→love 同期、ローカルファイルのスクロブルは同じタブにあります。
  未配達の再生記録は設定ファイルの隣の `scrobble-queue.jsonl` で待機します。

## Spotify インポート / エクスポート

Spotify、YouTube Music、プレーンなファイルの間でプレイリストを移動できます — チェックポイントと
再開つきのジョブ、あいまいな曲はマッチレポートへ:

```sh
ytt auth spotify --client-id <YOUR-CLIENT-ID>   # 初回のみ PKCE ブラウザ接続
ytt transfer list spotify                        # プレイリスト id を探す (list ytm も可)
ytt transfer import <spotify-url-or-id>          # → 新しい YTM プレイリスト
ytt transfer import liked --to likes             # Spotify のいいね → YTM のいいね (順序保持)
ytt transfer import <id> --to local:"ジム"        # → アプリ内ライブラリのプレイリストへ
ytt transfer import backup.csv --to-playlist "復元"   # Exportify CSV / ytm-tui JSON
ytt transfer export ytm:<id> --to spotify        # 逆方向
ytt transfer backup --dir ~/music-backup --csv   # 全 YTM プレイリスト → JSON (+CSV)
ytt transfer resume <job-id>                     # レート制限/中断後の再開
```

TUI の中でも: 設定 → **アカウント** → *Spotify からインポート…* でプレイリストを選ぶと、
音楽を流したままライブラリのプレイリストタブへ取り込みます。進行状況はステータス行に流れます。

**Spotify の設定（初回のみ）。** Development Mode の Spotify アプリは自分の許可リストにある
アカウントしか受け付けないので、自分の（無料の）アプリを作ります:
[developer.spotify.com/dashboard](https://developer.spotify.com/dashboard) でアプリを作成し、
リダイレクト URI に `http://127.0.0.1:9271/callback` を**正確に**追加し（ループバック IP、
`localhost` ではない; 9271 が使用中なら `spotify.redirect_port` で変更）、*User Management* に
自分の Spotify アカウントを追加して、アプリの Client ID を設定 → アカウントに貼ります（または
`ytt auth spotify --client-id …`）。クライアントシークレットはありません — PKCE フローは使わないので。

マッチングはメタデータベースです（NFKC 正規化、CJK 安全なタイトル + アーティスト + 長さ + アルバムの
タイブレーク）。許容スコア未満は黙って当てずっぽうにせず、ジョブレポートに *ambiguous* / *not found*
として残ります。`--take-best`、`--min-score` で再実行するか、手で直してください。
大きなプレイリストは `--dry-run` で確認してから `ytt transfer resume <job-id>` で書き込みを。

---

## 自分好みに

- **テーマ** — 内蔵13種（Default、Midnight、Light、High Contrast、Terminal Green、Gruvbox、Nord、Dracula、Tokyo Night、Solarized Dark、Rosé Pine、Dario、Retro）。さらに34の色ロールすべてが設定 → グラフィックで `#RRGGBB`（透明は `none`）を受け付けます。
- **EQ** — 設定 → 再生にある本物の10バンドグラフィック EQ（31 Hz–16 kHz）。**`e`** がプリセットを巡回し、**`N`** がラウドネスノーマライズを切り替えます。
- **アニメーション** — ナビの `✨`（または **`A`**）で全体をトグル。設定 → グラフィックで選べます: タイトルの煌めき、鼓動するハート、シークバーのグロー、EQ バー、マトリックスレイン、星空、DVD 風の跳ねるロゴ、そしてもちろん、くるくる回る ASCII ドーナツ。
- **レトロモード** — トグル一つ（設定 → グラフィック）ですべてが CP437 安全になります。素の Linux コンソールや年季の入った SSH セッション向け: Retro テーマ、ASCII 専用グリフ、そしてアルバムアートを正真正銘の ASCII アートで描き直します。
- **キー & マウス** — すべてのバインドが競合検知つきで再設定でき（設定 → ホットキー）、クリック派のために UI 全体がマウス対応です。

---

## トラブルシューティング

| 症状 | 対処 |
| --- | --- |
| 何も再生されない、再生でエラー | mpv か yt-dlp がないか `PATH` にありません。`ytt doctor` を実行。 |
| `ytt: command not found` | 新しいターミナルを開く。まだなら、インストーラが出力した `PATH` 行を追加。 |
| 昨日は動いたのに今日は動かない | YouTube が何か変えました — yt-dlp を更新（`brew upgrade yt-dlp`、`scoop update yt-dlp`、またはパッケージマネージャ）。 |
| 特定の曲だけ再生できない | サインインが必要かも — 下の Cookie 参照。 |
| アルバムアートが出ない | 初期設定はオフで、ターミナル依存です。**アルバムアート**（設定 → 一般）をオンにして再起動。 |
| `v` を押しても映像が出ない | 別の mpv ウィンドウを開く機能です — `ytt doctor` で mpv を確認。ローカル専用トラックには見せる映像がありません。 |
| コントロールセンター / SMTC / MPRIS に出ない | 設定 → 再生 → **OS メディアコントロール**を確認。何かが一度再生されてから表示されます。 |
| DJ Gem が反応しない | 設定 → DJ Gem に無料の Gemini キーを入れ、**Enable DJ Gem** をオンに。 |
| Spotify の接続/インポートで 403 | アプリが Development Mode です。開発者ダッシュボードの *User Management* に自分の Spotify アカウントを追加し、Client ID を再確認。 |
| スクロブルが反映されない | 設定 → アカウントが接続・有効か確認。オフライン分は自動配達されます（`scrobble-queue.jsonl` で待機）。デーモンは起動時にアカウントを読むので、接続後はデーモンを再起動。 |
| キーを変えすぎて混沌 | 設定 → 一般 → **ショートカットを初期化**。 |

まだ困っている？ [issue を立てて](https://github.com/Ochichan/ytm-tui/issues) OS を教えてください。

---

## サインイン & ファイルの場所

**Cookie（任意）。** ほぼ必要ありません — 公開曲は匿名で検索も再生もできます。メンバー限定/地域制限トラック（およびプレイリスト転送/アカウントのプレイリスト）に届くには、YouTube Music の Cookie を **Netscape 形式**の `cookies.txt` に書き出し（macOS: `~/Music/ytm-tui/cookies.txt`、Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`）、`ytt` を再起動してください。**そのファイルはパスワードのように扱うこと。** 設定 → 一般でパスを指定することもできます。

書き出しは*シークレットウィンドウ方式*でないと数分で死にます: **プライベート/シークレットウィンドウ**を開き、そこで music.youtube.com にサインインし、そのタブから `cookies.txt` を書き出して（先に拡張機能のシークレット許可を有効に）、**シークレットウィンドウを閉じます**。ブラウザが消えたセッションはローテーションもサインアウトもされません — 普段使いのブラウザからの書き出しはセッションがローテーションした瞬間に無効になり、ツールを酷使するとそのブラウザのログインまで切れることがあります。正しい書き出しには `SAPISID`/`SID` の行があります。訪問者のみ（未ログイン）の書き出しは動かず、`ytt` がそう教えてくれます。

**設定 & データ。**
- 設定: `~/Library/Application Support/ytm-tui/config.json`（macOS）· `~/.config/ytm-tui/config.json`（Linux）· `%APPDATA%\ytm-tui\config.json`（Windows）。
- その隣に: `playlists.json`（あなたのプレイリスト）、`scrobble-queue.jsonl`（未配達の再生記録）、`transfers/`（再開可能なジョブのチェックポイント + レポート）。
- ダウンロードの既定は `~/Music/ytm-tui`。**Download dir** 設定か `YTM_DOWNLOAD_DIR` で変更できます。
- `GEMINI_API_KEY` と `YTM_DOWNLOAD_DIR` 環境変数は、起動時に保存済み設定より優先されます。

---

## スペシャルサンクス

🙏 **[@ZZNN75](https://github.com/ZZNN75)** さんへ、本物の QA 時間に大きな感謝を — 隅々までつついて、わざと壊してくれたおかげで、あなたが壊す必要はありません。あなたが*出会わない*粗い角の多くは、この方が先にぶつかってくれたから滑らかなのです。🫡

## ライセンス

MIT。フォークして、出荷して、好きにどうぞ。
