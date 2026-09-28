//! Translations for the terminal interface (`ui.rs`).
//!
//! The bar here is conversational: if a sentence would not come out of a
//! person's mouth, it does not belong in this file.

/// `(english, 中文)` pairs.  Keys must match the literals used in `t!(...)`.
pub static ZH: &[(&str, &str)] = &[
    // -- tabs, manager list -------------------------------------------------
    (
        "no open windows — F9 manager, F1 help",
        "没有打开的窗口 — F9 管理器，F1 帮助",
    ),
    ("  saved connections", "  已保存的连接"),
    ("  persistent tasks", "  持久任务会话"),
    ("  open windows", "  打开的窗口"),
    ("settings discarded", "改动没保存，已丢弃"),
    (" sessions ", " 会话 "),
    (" details ", " 详情 "),
    ("password", "密码"),
    // -- manager detail: saved connection -----------------------------------
    ("  host      {}:{}", "  主机      {}:{}"),
    ("  user      {}", "  用户      {}"),
    ("(session cwd)", "（会话当前目录）"),
    ("  Enter   connect", "  回车   连接"),
    ("  e       edit settings (form)", "  e      改设置（表单）"),
    // -- manager detail: open window ----------------------------------------
    ("  host   {}", "  主机   {}"),
    ("  size   {}x{}", "  尺寸   {}x{}"),
    ("  status {}", "  状态 {}"),
    ("  cwd    {}", "  路径   {}"),
    ("(unknown)", "（未知）"),
    ("  Enter   switch to window", "  回车   切到这个窗口"),
    ("  q       close the window", "  q      关闭窗口"),
    // -- manager detail: remote session -------------------------------------
    ("  host        {}", "  主机        {}"),
    ("  supervisor  {}", "  守护进程    {}"),
    ("  age         {}", "  已运行      {}"),
    ("  verdict     {}", "  状态        {}"),
    (
        "  cpu         {}% (peak {}%)",
        "  cpu         {}%（峰值 {}%）",
    ),
    ("  memory      {}", "  内存        {}"),
    ("  processes   {}", "  进程        {}"),
    ("  last output {}s ago", "  最近输出    {}s 前"),
    (
        "  socket      {}   responsive {}   probe {}",
        "  socket      {}   响应 {}   探测 {}",
    ),
    ("  shell alive {}", "  shell 存活 {}"),
    (
        "  not measured yet — refreshing…",
        "  还没采样 — 正在刷新…",
    ),
    ("  state       {}", "  状态        {}"),
    ("attached", "已接入"),
    ("detached", "后台运行"),
    ("  Enter   reattach", "  回车   接回去"),
    ("  q   end this task", "  q   结束这个任务"),
    ("  nothing selected", "  什么都没选"),
    // -- terminal view ------------------------------------------------------
    ("no session — F9 to open one", "没有会话 — 按 F9 开一个"),
    ("[scrollback {}]", "[回看 {}]"),
    // -- help screen --------------------------------------------------------
    ("tabssh — keys", "tabssh — 快捷键"),
    ("quick actions", "快捷操作"),
    ("quit", "退出"),
    // -- help screen: the closing "about" note ------------------------------
    ("about", "关于"),
    (
        "tabssh is an extremely lightweight terminal SSH client for Windows,",
        "tabssh 是一个在 Windows 上连接 Linux Server 的极致轻量的终端 SSH 客户端。",
    ),
    (
        "connecting to Linux servers. Through the F2 command bar, Tab completion",
        "借助 F2 命令栏、Tab 补全与 GNU screen 持久任务管理，",
    ),
    (
        "and GNU screen persistent-task management, it feels more Linux-native and",
        "它提供了更贴近 Linux 原生风格的操作体验，",
    ),
    ("makes remote work more efficient.", "让远程工作更高效。"),
    // -- command bar / status ----------------------------------------------
    (
        "enter runs · Ctrl-C clears · Esc closes · drop a file here to fill in a put",
        "回车执行 · Ctrl-C 清空 · Esc 关闭 · 拖入文件会填好 put 命令",
    ),
    (
        "F2 command bar · F9 sessions · F1 help",
        "F2 命令栏 · F9 会话 · F1 帮助",
    ),
    // -- key handling -------------------------------------------------------
    (
        "type or drop a local path after 'put '",
        "在 put 后面输入或拖入一个本地路径",
    ),
    ("connecting to {}…", "正在连接 {}…"),
    (
        "no saved connection for {}; save one with F2 'new <name> <user@host>'",
        "没有 {} 的已保存连接；用 F2 'new <name> <user@host>' 存一个",
    ),
    ("reattaching to {}…", "正在接回 {}…"),
    ("switched to window {}", "已切到窗口 {}"),
    ("open a window first", "先打开一个窗口"),
    ("closing the current window", "正在关闭当前窗口"),
    // -- command bar commands ----------------------------------------------
    (
        "no saved connection for that host",
        "那台主机没有已保存的连接",
    ),
    ("Ctrl-S saves, Esc discards", "Ctrl-S 保存，Esc 放弃"),
    ("no such connection: {}", "没有这个连接: {}"),
    ("usage: edit <name>", "用法: edit <name>"),
    ("downloading {} into {}…", "正在下载 {} 到 {}…"),
    ("open a session first", "先打开一个窗口"),
    ("usage: get <remote-path>", "用法: get <remote-path>"),
    (
        "usage: put <local-path> … (or drag a file into this bar)",
        "用法: put <local-path> …（也可以把文件拖到这行里）",
    ),
    ("unknown command: {}", "不认识的命令: {}"),
    ("uploading {} item(s)…", "正在上传 {} 项…"),
    // -- settings form: sections, labels, hints -----------------------------
    ("network", "网络"),
    ("connection", "连接"),
    ("transfer", "传输"),
    ("behaviour", "行为"),
    ("name", "名称"),
    ("host", "主机"),
    ("port", "端口"),
    ("user", "用户名"),
    ("upload dir", "上传目录"),
    ("download dir", "下载目录"),
    ("screen status", "screen 状态"),
    ("installed", "已安装"),
    ("not installed", "未安装"),
    ("not tested yet", "尚未检测"),
    ("install screen", "安装 screen"),
    ("press Enter to copy this command", "按回车复制这行命令"),
    ("copied to the clipboard: {}", "已复制到剪贴板：{}"),
    ("copied {} chars", "已复制 {} 个字符"),
    ("no text on the clipboard", "剪贴板里没有文本"),
    ("could not copy: {}", "复制失败：{}"),
    ("note", "备注"),
    ("label in the session list", "会话列表里显示的名字"),
    ("hostname or ip", "主机名或 IP"),
    ("default 22", "默认 22"),
    (
        "remote path — blank = session's current directory, then home",
        "远端路径 — 留空 = 会话当前目录，再退回 home",
    ),
    (
        "local path — blank = the command bar's directory (starts at the desktop)",
        "本地路径 — 留空 = 命令栏当前目录（初始是桌面）",
    ),
    // -- settings form: values, headings, validation ------------------------
    ("(none)", "（无）"),
    ("(not set)", "（未设置）"),
    (
        "(session's current directory, then home)",
        "（会话当前目录，再退回 home）",
    ),
    ("new connection", "新建连接"),
    ("connection settings", "连接设置"),
    ("cannot save: {}", "存不了: {}"),
    ("saved connection {}", "已保存连接 {}"),
    ("updated connection {}", "已更新连接 {}"),
    (
        "port must be a whole number 1-65535",
        "端口要填 1-65535 的整数",
    ),
    // -- tab completion -----------------------------------------------------
    ("no command matches {}", "没有匹配 {} 的命令"),
    ("nothing on the host matches {}", "远端没有匹配 {} 的"),
    ("nothing local matches {}", "本地没有匹配 {} 的"),
    ("that needs a connected window", "需要一个已连接的窗口"),
    // -- the command list, now plain words ----------------------------------
    // -- one key field, no auth modes ---------------------------------------
    ("key", "密钥"),
    (
        "a path, or paste the key itself — optional",
        "填路径，或直接粘贴密钥内容 — 可以不填",
    ),
    (
        "used automatically if the host asks for one",
        "主机要密码时会自动用上",
    ),
    (
        "tested over a background connection when you connect",
        "连接时用后台连接自动检测",
    ),
    // -- one unified command list -------------------------------------------
    ("command bar — F2", "命令栏 — F2"),
    ("this help", "本帮助"),
    ("command bar", "命令栏"),
    ("next window", "下一个窗口"),
    ("previous window", "上一个窗口"),
    ("refresh remote cwd", "刷新远端目录"),
    ("upload files (or drag & drop)", "上传文件（也可拖进来）"),
    ("close / detach window", "关闭窗口"),
    ("session manager", "会话管理器"),
    ("download a file or directory", "下载文件或目录"),
    (
        "upload to the session's current directory",
        "上传到会话当前目录",
    ),
    (
        "Tab completes names and paths everywhere",
        "任何地方按 Tab 都能补全名称和路径",
    ),
    ("no saved connection matches {}", "没有匹配 {} 的已保存连接"),
    ("no session matches {}", "没有匹配 {} 的会话"),
    (
        "check the settings, then Ctrl-S to save",
        "看一眼设置，按 Ctrl-S 保存",
    ),
    (
        "  none yet — F2, then new <user@host[:port]>",
        "  还没有连接 — 按 F2，输入 new <用户@主机[:端口]>",
    ),
    // -- the grouped command list -------------------------------------------
    ("connections", "连接"),
    ("files", "文件"),
    ("sessions", "会话"),
    ("this program", "本程序"),
    (
        "new connection — the name is chosen for you",
        "新建连接，名字自动分配",
    ),
    ("change its settings", "改设置"),
    ("forget it", "从列表里删掉"),
    ("opening a connection", "打开连接"),
    ("open a regular shell connection window", "打开一个常规 shell 连接窗口"),
    ("create a new persistent task", "创建一个新的持久任务"),
    (
        "the task keeps running after you disconnect; reattach it from the manager (F9)",
        "断线后任务还在跑；之后到管理器（F9）里接回去",
    ),
    (
        "close a window, end a task, or forget a connection",
        "关窗口、结束任务，或删掉连接",
    ),
    ("open the session manager (same as F9)", "打开会话管理器（同 F9）"),
    ("close the current window", "关闭当前窗口"),
    (
        "close the current window but leave the task running",
        "关闭当前窗口，但任务继续运行",
    ),
    (
        "scroll the terminal's scrollback",
        "滚动终端的回看内容",
    ),
    (
        "select text; releasing copies it to the clipboard",
        "拖动选择文本；松开即复制到剪贴板",
    ),
    ("paste the clipboard", "粘贴剪贴板内容"),
    (
        "your own note — saved with the connection",
        "你自己的备注 — 跟连接一起保存",
    ),

    ("switch language", "切换语言"),
    // -- the cleanup command and the language setting -----------------------
    ("closing window {}", "正在关闭窗口 {}"),
    (
        "restarting window {} — task {} ended",
        "正在重启窗口 {} — 任务 {} 已结束",
    ),
    ("task ended — fresh shell", "任务已结束 — 换成了新 shell"),
    ("killing task {}", "正在结束任务 {}"),
    ("the session was not open", "这个会话本来就没打开"),
    ("could not restart the window: {}", "重启窗口失败：{}"),
    // -- the screen extension ------------------------------------------------
    (
        "server screen task {} is running — a dropped connection does not stop it",
        "服务器 screen 任务 {} 正在运行 — 断线也不会停止它",
    ),
    ("screen: {}", "screen：{}"),
    ("forgot the saved connection {}", "已删掉连接 {}"),
    ("nothing called {}", "没有叫 {} 的东西"),
    ("no session {}", "没有 {} 号会话"),
    ("language set to {}", "语言已设为 {}"),
    ("following the system language: {}", "跟随系统语言：{}"),
    (
        "language {} ({}) — use setlang auto|en|zh",
        "语言 {}（{}）— 用 setlang auto|en|zh 切换",
    ),
    ("set by you", "手动设置"),
    ("following the system", "跟随系统"),
    (" help — scroll with ↑ ↓ ", " 帮助 — 用 ↑ ↓ 滚动 "),
    // -- the local file browser ---------------------------------------------
    ("tab completion", "Tab 补全"),
    ("local", "本机"),
    ("list a local directory", "列出本机目录"),
    ("go to a local directory", "切换本机目录"),
    ("no such directory: {}", "没有这个目录：{}"),
    ("cannot read: {}", "读不了：{}"),
    ("now in {}", "已进入 {}"),
    (
        "closed {} window(s) and forgot {}",
        "关掉了 {} 个窗口，并删掉连接 {}",
    ),
    (
        "no such local path: {} — press Tab to list what is here, or cd first",
        "没有这个本地路径：{} — 按 Tab 看看这儿有什么，或者先 cd",
    ),
    ("press Enter to upload this", "按回车上传"),
    // -- the host-key ruling (modal) ------------------------------------------
    ("host key", "主机密钥"),
    ("new host key for {}", "主机 {} 的新密钥"),
    ("WARNING: the host key for {} changed", "警告：主机 {} 的密钥变了"),
    ("saved:   {}", "已保存：{}"),
    ("offered: {}", "收到的：{}"),
    ("y trust & save · n refuse", "y 信任并保存 · n 拒绝"),
    ("host key for {} saved", "已保存主机 {} 的密钥"),
    ("host key for {} refused", "已拒绝主机 {} 的密钥"),
    // -- the screen-session form (manager `e` on a task) --------------------
    ("screen session", "screen 会话"),
    ("session name", "会话名称"),
    ("window title", "窗口标题"),
    ("detach clients", "断开所有客户端"),
    ("the session's name on the host", "会话在主机上的名字"),
    (
        "the screen window's title — leave blank to keep it",
        "screen 窗口的标题 — 留空就不改",
    ),
    ("detach every client when you save", "保存时断开所有客户端"),
    ("(unchanged)", "（不改）"),
    ("nothing changed", "没有改动"),
    ("no live connection to that host", "那台主机没有活动连接"),
    (
        "no live connection to that host — open a window to it first",
        "那台主机没有活动连接 — 先开一个它的窗口",
    ),
    ("updating session {}…", "正在更新会话 {}…"),
    ("session {} updated", "会话 {} 已更新"),
    ("could not change the session: {}", "改不了这个会话：{}"),
    ("  e   edit the session", "  e   改会话信息"),
    (
        "this console cannot show the interface — run tabssh in Windows Terminal, or a Windows 10 or newer console",
        "这个控制台显示不了界面 — 请在 Windows Terminal 或 Windows 10 及以上自带的控制台里运行 tabssh",
    ),
    ("console:    {}", "控制台:     {}"),
    ("modern terminal", "现代终端"),
    ("classic console (ANSI enabled)", "经典控制台（已启用 ANSI）"),
    ("no ANSI support", "不支持 ANSI"),
];
