# 手動驗收清單（需實體硬體）

> 本清單集中列出**必須有實體硬體才能驗證**的項目——mock device 治具（T0.2，見
> `crates/mock-device/`）刻意無法覆蓋這些項目，因為它們測的正是「真實
> termios/ioctl/USB 行為」本身，而不是 daemon 邏輯。CI 不會跑這些項目；每項
> 完成後由執行者手動勾選並附上驗證紀錄（日期、平台、裝置型號、實際輸出）。
>
> 這份清單從 `TASKS.md` 各任務的「合格標準」與 GitHub `needs:hardware`
> label 的 issue 收集而來。新任務若又冒出需要實機的驗收項，加進這裡，不要
> 散落在別處。

## 如何使用

- 每項勾選前必須**本人在指定平台上實際跑過**，不接受推想或「理論上應該通過」。
- 勾選時在該項下方補一行：`驗證紀錄：YYYY-MM-DD，平台，裝置，結果摘要`。
- 有實機環境的協作者請認領項目；沒有硬體的協作者可以跳過，不影響其餘 CI 綠燈的任務推進。

---

## 1. 自訂 Baud Rate（74880）

對應：T1.3 Port I/O 與設定核心 — [issue #5](https://github.com/SheldonChangL/serialwrap/issues/5)（`needs:hardware`）

74880 是 ESP8266 開機 log 的預設 baud，是最常見、也最容易在跨平台 API 上出錯的
非標準值（`serialport` crate 在 macOS 走 `IOSSIOSPEED`、Linux 走
`BOTHER` termios2，兩條路徑都需要實測才能確定真的支援任意值而非只支援清單內
的標準 baud）。

- [ ] macOS：自訂 baud 74880 實測通過
  - 所需硬體：任意 USB-serial 轉接器（CH340/CP2102/FTDI 皆可）或 ESP8266/ESP32 開發板
  - 判定通過：以 74880 開啟 port，對端（loopback 或裝置實際開機 log）收到的內容可讀、無亂碼；用 `stty -f /dev/cu.xxx speed` 或等效方式確認驅動回報的實際 speed 與設定值一致（非四捨五入到最近標準值）
  - 驗證紀錄：
- [ ] Linux：自訂 baud 74880 實測通過
  - 所需硬體：同上
  - 判定通過：同上；Linux 額外確認 `BOTHER`/`ASYNC_SPD_CUST` 路徑生效（非 fallback 到最近標準 baud）
  - 驗證紀錄：

## 2. 「不觸碰 DTR/RTS」開啟不觸發 Arduino Uno 自動 reset

對應：T1.3 Port I/O 與設定核心 — [issue #5](https://github.com/SheldonChangL/serialwrap/issues/5)（`needs:hardware`）

Arduino Uno（及多數用 DTR 觸發 reset 進 bootloader 的板子）在 serial port 被
`open()` 時，若 DTR 被驅動預設拉低再拉高，會觸發 MCU reset。「不觸碰
DTR/RTS」模式必須讓 daemon 開啟 port 時完全不改變這兩條控制線的既有狀態，這件
事只能用真的會被 reset 觸發的板子驗證——mock PTY 沒有 DTR/RTS 這回事。

- [ ] macOS：以「不觸碰 DTR/RTS」模式開啟 Arduino Uno，不觸發 reset
  - 所需硬體：Arduino Uno（或任何以 DTR 觸發 auto-reset 的板子）
  - 判定通過：開啟 port 前先讓板子印出一段可辨識的計數器 log；用「不觸碰」模式開啟 port 後，計數器連續不中斷（沒有從頭重新開機的 boot banner 出現）
  - 驗證紀錄：
- [ ] Linux：同上
  - 所需硬體：同上
  - 判定通過：同上
  - 驗證紀錄：
- [ ] 兩平台：`dtr_pulse`（獨立顯名操作，見 issue #5）確實觸發板子 reset，且方向（先拉低再拉高）與該板子的實際 auto-reset 電路相符
  - 所需硬體：Arduino Uno（或任何以 DTR 觸發 auto-reset 的板子）
  - 判定通過：呼叫 `dtr_pulse` 後板子重新開機（boot banner 重新出現）；若特定板子的極性相反，記錄下來供之後調整預設方向
  - 驗證紀錄：

## 3. `serialwrap run -- esptool` 實機燒錄

對應：T2.2 Lease 模式 `serialwrap run --` — [issue #9](https://github.com/SheldonChangL/serialwrap/issues/9)（`needs:hardware`）

Lease 機制（daemon 讓出 port fd 給子行程獨佔）只有跑一次真正的燒錄流程才能
證明「daemon 收回 port、恢復錄製、期間其他 client 沒有斷線」全部成立；mock
device 治具（`crates/serialwrapd/tests/lease.rs`、`lease_protocol.rs`、
`crates/serialwrap/tests/run_cli.rs`）已經涵蓋狀態機（fd 讓出/收回的時序、
含共享 fd 斷言）、事件欄位與起訖時間、follow 不斷線、子行程 SIGKILL、
`--lease-timeout`、daemon 重啟後殘留 lease 收回——這些都不必重複用實機驗證。
這裡剩下、也只有實機才測得出來的，是 esptool 這種真實工具對 port 的實際
操作模式（開啟方式、DTR/RTS timing、baud 切換順序等）是否被正確相容。

- [ ] macOS：`serialwrap run -- esptool.py write_flash ...` 完整燒錄一次成功
  - 所需硬體：ESP8266/ESP32 開發板
  - 前置注意：`serialwrap run` 不會自動幫 esptool 補上 `--port`；它會把 daemon
    交出的裝置路徑存進子行程的 `SERIALWRAP_LEASE_PATH` 環境變數，實測時自己組
    指令，例如 `serialwrap run -- esptool.py --port "$SERIALWRAP_LEASE_PATH" write_flash 0x0 firmware.bin`
    （或直接手動填已知的 `/dev/cu.*`／`/dev/ttyUSB*` 路徑）。
  - 判定通過：燒錄完成、esptool 回報成功；燒錄後裝置 boot log 被完整錄到（對應 S3 出口情境）；燒錄期間另一個 client 的 `tail -f` 收到 lease 事件而非斷線
  - 驗證紀錄：
- [ ] Linux：同上
  - 所需硬體：同上
  - 判定通過：同上
  - 驗證紀錄：

## 4. 桌面通知

對應：T4.2 審批流程與通知 — [issue #15](https://github.com/SheldonChangL/serialwrap/issues/15)（`needs:hardware`，嚴格說是「需要桌面環境」而非硬體，但同樣無法在無頭 CI 上驗）

- [ ] macOS：審批請求觸發 `osascript`（或 terminal-notifier）桌面通知，實際看到彈出
  - 所需硬體/環境：有登入 GUI session 的 macOS 桌面
  - 判定通過：觸發一次 gate pending 事件，畫面上出現系統通知；通知內容含 requester 與命中規則
  - 驗證紀錄：
- [ ] Linux：同上情境觸發 `notify-send`，實際看到彈出
  - 所需硬體/環境：有登入桌面環境（GNOME/KDE 等，需 D-Bus notification daemon）的 Linux
  - 判定通過：同上
  - 驗證紀錄：
- [ ] 兩平台：通知機制本身失敗（例如背景無桌面 session）時，審批流程仍正常運作（不因通知失敗而卡住或 panic）
  - 判定通過：拔掉/停用通知後端，pending 仍可經 CLI list/approve/deny 正常完成
  - 驗證紀錄：

## 5. Linux `TIOCGICOUNT` 錯誤計數；macOS 顯示 unavailable

對應：T1.3 Port I/O 與設定核心 — [issue #5](https://github.com/SheldonChangL/serialwrap/issues/5)（`needs:hardware`）

Linux 的 framing/overrun/parity error 計數走 `TIOCGICOUNT` ioctl；macOS 沒有對應
機制。這裡要驗證兩件事：Linux 上計數真的隨錯誤增加、macOS 上介面誠實回報
「不可用」而不是靜默顯示 0（顯示 0 會被誤讀為「沒有錯誤發生」）。

- [ ] Linux：故意製造 framing/parity error（例如錯誤 baud 對接非同 baud 的裝置），確認 `TIOCGICOUNT` 計數確實遞增
  - 所需硬體：兩台可用不同 baud 對接的 UART 裝置，或可控制錯誤注入的 USB-serial 轉接器
  - 判定通過：CLI/事件流中錯誤計數在製造錯誤後可觀察到遞增
  - 驗證紀錄：
- [ ] macOS：確認錯誤計數欄位顯示為「unavailable」而非 `0`
  - 所需硬體：任意 macOS 機器＋ USB-serial 裝置
  - 判定通過：CLI/事件流輸出明確標示 unavailable（字串或專屬列舉值），不是數字 0
  - 驗證紀錄：

## 6. 打包安裝與服務自啟（乾淨 VM、launchd、systemd）

對應：T6.1 打包與服務安裝 — [issue #23](https://github.com/SheldonChangL/serialwrap/issues/23)（`needs:hardware`）

這裡的 mock-device 治具原理上就測不出「一台從沒裝過開發環境的機器，從零開始能不能在
15 分鐘內裝起來看到 log」——這個問題的答案恰恰取決於治具刻意繞過的東西：相依套件是否
齊全、文件步驟有沒有漏、真正的桌面/init 系統能不能把 daemon 拉起來。以下區分「這次
session 已經自動驗過的部分」與「仍需要人在真實環境跑一次的部分」，不要混為一談。

**已驗過（本次 session，非人工）：**

- [x] Linux（Docker `ubuntu:24.04`，非乾淨 VM，見下方限制）：從乾淨映像跑 `apt`
  安裝相依套件 → `rustup` 裝 Rust → Node 22 → `npm run build`（前端）→ `cargo
  build --release` → `cargo test --all`（364+ 個測試，mock-device 全流程）→ 啟動
  daemon → 透過 CLI `tail` 在螢幕上看到 log 行，總計 **~135 秒**（遠低於 15
  分鐘門檻）。完整指令序列與逐段計時見 PR 說明；`packaging/linux/install.sh`
  單獨用同一個 base image 重跑一次，66 秒完成（複用已裝好 Rust/Node 的 base
  image，故比上面全流程數字快，公平比較應看上面含 apt/rustup/node 安裝的
  135 秒）。
  - **這不等於通過「乾淨 Ubuntu VM」那一項**：Docker 容器沒有真實 USB 裝置、
    沒有桌面 session（GUI 瀏覽器打開 `http://127.0.0.1:5590` 這一步沒有實測，
    只驗了背後的 HTTP API 有回應）、也沒有真實開機/reboot 流程可測。它驗證的
    是「相依套件是否齊全、install script 能不能跑、文件步驟有沒有缺漏」，這正
    是 Docker 測法的價值所在，但不能取代下面兩項乾淨 VM 待驗項目。
- [x] `serialwrap service install --dry-run` 產出的 launchd plist（macOS，本機
  原生跑，非容器）：`plutil -lint` 驗證合法 XML；內容含正確的
  `ProgramArguments`（binary 絕對路徑 + `daemon`）、`RunAtLoad`/`KeepAlive`。
- [x] `serialwrap service install --dry-run` 產出的 systemd user unit（Linux，
  Docker）：內容含正確的 `ExecStart`（binary 絕對路徑 + `daemon`）、
  `WantedBy=default.target`；`service install`（非 dry-run，假 `$HOME`）確認
  檔案真的被寫入，且在容器內沒有真正 systemd session 時，`systemctl --user
  daemon-reload` 失敗會回傳明確錯誤而不是靜默假裝成功。
- [x] `cargo deb -p serialwrap` 產出的 `.deb`（Docker，`x86_64` 目標，與
  release workflow 同架構）：`dpkg-deb -c`/`-I` 確認檔案佈局正確
  （`usr/bin/serialwrap`、`lib/udev/rules.d/60-serialwrap.rules`、
  `usr/share/doc/serialwrap/README.md`）與 control metadata 正確（package
  name、maintainer、`Depends: libc6 (>= 2.39)` 自動偵測）。

**仍待人工在真實環境驗證：**

- [ ] 乾淨 macOS VM：完全沒跑過。本 session 沒有 macOS VM 可用，只能在「已經
  裝好開發環境的本機」上驗證個別指令（`cargo build`、`service install
  --dry-run` 等），無法驗證「從零開始的乾淨環境」這個條件本身，也無法驗證
  CH340/CP210x 驅動安裝與核可流程（System Settings 的核可對話框）。
  - 判定通過：照 README 從安裝到 GUI 看到 log ≤15 分鐘（實測計時＋畫面截圖）
- [ ] 乾淨 Ubuntu VM（非容器，真實或虛擬機）：Docker 測法涵蓋了「相依套件／
  安裝流程／文件完整性」，但沒有真實桌面 session、沒有真實 USB 裝置插拔、沒有
  真實 `systemd --user` session（見上方限制說明）。
  - 判定通過：照 README 從安裝到瀏覽器看到 GUI log ≤15 分鐘（實測計時＋畫面截圖），且用真實 USB-serial 裝置而非本 session 用的內部測試後門
- [ ] 重開機後 daemon 自動啟動並恢復錄製 — macOS（launchd）
  - 所需硬體/環境：乾淨 macOS 機器或 VM，登入 GUI session
  - 判定通過：`serialwrap service install` 後重開機，不需手動操作，`serialwrap devices`/GUI 顯示 daemon 已在跑且先前錄製的資料還在
- [ ] 重開機後 daemon 自動啟動並恢復錄製 — Linux（systemd --user）
  - 所需硬體/環境：乾淨 Ubuntu 機器或 VM；須先跑過 `loginctl enable-linger
    "$USER"`（README 已提示這一步，沒有它 user unit 只在有登入 session 時才會
    啟動，不會在開機當下就跑）
  - 判定通過：重開機（不登入任何 session）後 `systemctl --user status
    com.serialwrap.daemon.service` 顯示 running，且先前錄製的資料還在
- [ ] macOS 常見驅動（CH340/CP210x）指引本身是否對得上真實核可流程
  - 判定通過：照 README「macOS: 常見 USB-serial 驅動」段落安裝後，`serialwrap devices` 真的看得到裝置；記下實際核可對話框的位置是否與文件描述相符（Apple 各版本措辭常變動）

---

## 其餘 `needs:hardware` 項目（收集自現有 issue，供後續任務認領時對照）

以下項目在對應任務開工時應併入該任務自己的驗收流程，這裡先登記避免遺漏。

### T1.1 裝置識別與熱插拔 — [issue #3](https://github.com/SheldonChangL/serialwrap/issues/3)

- [ ] macOS 實機驗證一律使用 `/dev/cu.*` 節點且開啟不阻塞（不是 `/dev/tty.*`）
  - 所需硬體：任意 USB-serial 轉接器
  - 判定通過：`serialwrapd` 開啟裝置時使用的路徑經確認是 `cu.*`；反覆插拔不出現因等待 DCD 而卡住的開啟

---

## 明確排除：這些不需要進本清單

- 任何能用 mock-device 治具（PTY）重現的行為——協定並發、錄製正確性、
  context 保護層折疊/截斷、gate 規則判定——一律走自動化測試，不得因為「有實機更放心」
  而重複列進手動清單。手動清單只留給治具原理上測不出來的項目（真實 baud 時脈、
  真實 DTR/RTS 電氣行為、真實桌面通知後端、真實 USB 熱插拔）。
