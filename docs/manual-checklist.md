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

---

## 其餘 `needs:hardware` 項目（收集自現有 issue，供後續任務認領時對照）

以下項目在對應任務開工時應併入該任務自己的驗收流程，這裡先登記避免遺漏。

### T1.1 裝置識別與熱插拔 — [issue #3](https://github.com/SheldonChangL/serialwrap/issues/3)

- [ ] macOS 實機驗證一律使用 `/dev/cu.*` 節點且開啟不阻塞（不是 `/dev/tty.*`）
  - 所需硬體：任意 USB-serial 轉接器
  - 判定通過：`serialwrapd` 開啟裝置時使用的路徑經確認是 `cu.*`；反覆插拔不出現因等待 DCD 而卡住的開啟

### T6.1 打包與服務安裝 — [issue #23](https://github.com/SheldonChangL/serialwrap/issues/23)

- [ ] 乾淨 macOS VM：照 README 從安裝到 GUI 看到 log ≤15 分鐘（實測計時）
- [ ] 乾淨 Ubuntu VM：同上
- [ ] 重開機後 daemon 自動啟動並恢復錄製（兩平台）
  - 所需硬體/環境：乾淨的 macOS 與 Ubuntu VM 或實體機（不可用已裝過開發環境的機器，會遮蔽遺漏的相依套件）
  - 判定通過：計時器與畫面截圖佐證

---

## 明確排除：這些不需要進本清單

- 任何能用 mock-device 治具（PTY）重現的行為——協定並發、錄製正確性、
  context 保護層折疊/截斷、gate 規則判定——一律走自動化測試，不得因為「有實機更放心」
  而重複列進手動清單。手動清單只留給治具原理上測不出來的項目（真實 baud 時脈、
  真實 DTR/RTS 電氣行為、真實桌面通知後端、真實 USB 熱插拔）。
