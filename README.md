Tab completes names and paths everywhere.

Tabssh is a tiny, Linux-native-style open SSH tool for win10/win11. It speaks ssh itself (via russh) — it does not call the system ssh and links no third-party runtime DLLs.

Keys & commands (the F1 page)

Quick actions

F1 - this help
F2 - command bar
F3 / F4 - next / previous window
F5 - refresh remote cwd
F6 - upload files (or drag & drop)
F8 - close / detach window
F9 - session manager
F10 - quit
PgUp / PgDn - scroll the terminal's scrollback
mouse drag - select text; releasing copies it to the clipboard
right click - paste the clipboard

Opening a connection (in the F9 manager)

Enter - open a regular shell connection window
Tab - create a new persistent task - it keeps running after you disconnect; reattach it from the manager (F9)

Command bar - F2

new [<name>] <user@host[:port]> - new connection - the name is chosen for you
edit <name> - change its settings
quit <name> - forget it
ls [dir] - list a local directory
cd [dir] - go to a local directory
get <remote-path> - download a file or directory
put <local-path> ... - upload to the session's current directory
quit [n|task|name] - close a window, end a task, or forget a connection
setlang auto|en|zh - switch language


快捷键与命令（F1 帮助页）

快捷操作

F1 - 本帮助
F2 - 命令栏
F3 / F4 - 下一个 / 上一个窗口
F5 - 刷新远端目录
F6 - 上传文件（也可拖进来）
F8 - 关闭窗口
F9 - 会话管理器
F10 - 退出
PgUp / PgDn - 滚动终端的回看内容
鼠标拖动 - 拖动选择文本；松开即复制到剪贴板
右键 - 粘贴剪贴板内容

打开连接（在 F9 管理器里）

回车 - 打开一个常规 shell 连接窗口
Tab - 创建一个新的持久任务 - 断线后任务还在跑；之后到管理器（F9）里接回去

命令栏 - F2

new [<名字>] <用户@主机[:端口]> - 新建连接，名字自动分配
edit <名字> - 改设置
quit <名字> - 从列表里删掉
ls [目录] - 列出本机目录
cd [目录] - 切换本机目录
get <远端路径> - 下载文件或目录
put <本地路径> ... - 上传到会话当前目录
quit [编号|任务|名字] - 关窗口、结束任务，或删掉连接
setlang auto|en|zh - 切换语言
