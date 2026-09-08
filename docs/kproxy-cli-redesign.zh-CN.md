# kproxy CLI 改造方案

状态：已按最终命令结构实施，尚未发布。

基线：2026-09-08，仓库提交 `dc86a6b`，workspace 版本 `0.2.4`，Clap 锁定为 `4.5.16`。文中的源码路径相对仓库根目录；“改造前”均指该基线。

## 1. 改造目标与交付边界

统一规则：命令组负责展示可执行操作，叶子命令负责执行操作。用户看到 `kproxy logs`，就能继续找到日志查询、跟踪、链路和文件入口。

本轮覆盖命令解析、帮助导航、旧语法移除、Shell 补全、Docker wrapper、相关文档和验证。账号、代理、统计、日志等业务执行逻辑继续复用现有实现和 IPC 方法；现有业务输出结构、日志来源、筛选含义、默认数量和确认流程保持稳定。

保留现有命令名称和层级，包括 `account`、`models`、`model-map` 等名称，以及已有的 `rm`、`delete`、`create` 等别名。本轮不增加新的资源分组层级，也不进行全局命名清理。

本次按用户要求直接实现统一入口，不保留旧 `logs`、`models`、`alert` 参数或隐藏命令写法。当前改动不包含发布、打 tag 或版本号调整，发布说明仍需明确旧入口已被移除。

## 2. 改造前的具体问题

| 位置 | 已核对的现状 | 本轮处理 |
| --- | --- | --- |
| `crates/kproxy/src/main.rs` 的 `Command` 及分发 | `logs`、`models`、`tasks`、`diagnose` 的子命令为可选；省略时执行业务 | 补齐显式动作，统一无参入口 |
| 同文件的 `LogsCommand` | 已有 `show`、`follow`、`trace`、`files`、`path`，在 `logs --help` 中可见 | 保留这五个入口，改善到达它们的路径 |
| 同文件的 `Help` | 手动禁用了 Clap 默认 help 子命令；只接受一个主题字符串 | 支持任意深度命令帮助及完整命令树 |
| `crates/kproxy/src/commands/runtime.rs` 的 `HELP_TOPICS`、`print_topic` | 独立维护主题清单和说明，与真实命令树不同步 | 主题说明迁至 guide，命令导航来自 Clap 定义 |
| `crates/kproxy/src/main.rs` 的启动顺序 | 先加载 `.env`，再解析帮助 | 纯导航先执行，业务参数仍在加载 `.env` 后完成解析 |
| `deploy/kproxy-docker` | 普通帮助也要先找到运行中的业务容器 | 增加停止状态下的 CLI 导航通道 |
| `crates/kproxy/src/commands/account.rs` | `add-sso-batch` 被明确标记为隐藏，已有 `add-sso --batch` 公开入口 | 删除隐藏入口，只保留 `add-sso --batch` |
| README 和启动文档 | 混用 `tasks`、`models --refresh`、`logs` 等默认写法 | 全部迁移为显式动作 |

这里需要解决的主要是公开功能的可发现性。所有功能都通过组帮助、完整命令树和补全找到，不维护隐藏的旧入口。

## 3. 目标命令结构

以下公开结构已经实现。

| 命令组 | 公开子命令 | 改动 |
| --- | --- | --- |
| `logs` | `show`、`follow`、`trace`、`files`、`path` | 保留子命令；无参改为组帮助 |
| `models` | `list`、`resolve` | 新增 `list`；无参改为组帮助 |
| `tasks` | `list`、`run` | 新增 `list`；无参改为组帮助 |
| `diagnose` | `all`、`endpoints`、`account` | 新增 `all`；无参改为组帮助 |
| `account` | `list`、`show`、`import`、`export`、`add-api-key`、`add-sso`、`rm`、`enable`、`disable`、`tag`、`regen-machine-id`、`refresh`、`probe`、`reset-health` | 保留动作；统一组帮助 |
| `config` | `list`、`show`、`path`、`reload`、`edit`、`reset`、`validate` | 保留动作；统一组帮助 |
| `apikey` | `list`、`show`、`add`、`rm`、`enable`、`disable`、`limit`、`usage`、`history`、`reset-usage` | 保留动作；统一组帮助 |
| `service` | `list`、`show`、`create`、`edit`、`enable`、`disable`、`delete`、`apikeys` | 保留动作；统一组帮助 |
| `alert` | `config`、`events`、`platforms`、`list`、`add`、`edit`、`delete`、`test`、`logs` | 保留动作；统一组帮助 |
| `model-map` | `list`、`add`、`edit`、`delete`、`test` | 保留动作；统一组帮助 |

表内仅列主名称，已有可见别名保留。`alert logs` 是查询告警投递记录的叶子命令，继续直接执行。

直接执行的查询命令保留：`status`、`stats`、`pool`、`health`、`ready`、`subscriptions`、`version`。宿主机生命周期命令 `restart`、`stop`、`uninstall` 保留当前执行范围和确认规则。

导航命令为 `help [COMMAND_PATH...] [--all]`、新增的 `guide [TOPIC]` 和新增的 `completions <bash|zsh|fish>`。

三个新增业务入口的行为如下：

| 新入口 | 行为与参数 | 复用位置 |
| --- | --- | --- |
| `models list [--mapped] [--refresh]` | 默认列出当前模型，并完整覆盖原来的映射和刷新能力 | `main.rs` 中原 `Models` 的无子命令分支及 `runtime::show_models` |
| `tasks list` | 返回当前周期任务状态，输出与旧 `tasks` 相同 | 现有 `method::TASKS` |
| `diagnose all` | 执行原无参诊断的端点检查和全部账号真实推理检查 | 现有 `DIAGNOSE_ENDPOINTS`、`DIAGNOSE_ACCOUNT` |

`diagnose all` 默认沿用 `us-east-1`、单账号超时 `45s`、并发 `1`，并可复用现有 `--region`、`--timeout`、`--concurrency/-c` 参数来调整。仍先检查端点，再检查账号；保留原错误传播和结果结构，避免入口改造同时改变诊断行为。

`diagnose all` 的帮助应明确说明会对全部账号发起真实推理。`diagnose account` 继续要求账号标识或 `--all`；缺少目标属于参数错误。

## 4. 入口切换

| 输入 | 改造前 | 当前实现 |
| --- | --- | --- |
| `kproxy` | 总帮助 | 统一后的总帮助 |
| `kproxy logs` | 查询近期日志 | 日志组帮助 |
| `kproxy logs show --tail 100` | 无此显式入口 | 查询日志 |
| `kproxy logs follow` | 无此显式入口 | 持续跟踪日志 |
| `kproxy models` | 列出模型 | 模型组帮助 |
| `kproxy models list --mapped --refresh` | 无此显式入口 | 刷新并列出模型与映射 |
| `kproxy tasks` | 列出任务 | 任务组帮助 |
| `kproxy tasks list` | 无此显式入口 | 列出任务 |
| `kproxy diagnose` | 完整诊断 | 诊断组帮助 |
| `kproxy diagnose all` | 无此显式入口 | 完整诊断 |
| `kproxy account add-sso-batch --file FILE` | 隐藏入口 | 参数错误 |
| `kproxy account add-sso --batch FILE` | 公开入口 | 批量 SSO 登录 |
| `kproxy alert add --kind ... --url ...` | 旧参数别名 | 参数错误；使用 `--platform` 和 `--webhook-url` |
| `kproxy help balance` | 主题说明 | 参数错误；使用 `kproxy guide balance` |

旧命令参数不做转换：`logs --tail/--level/--account/-f/--follow`、`models --mapped/--refresh`、`account add-sso-batch --file`、`alert --kind/--url` 和平台值 `wechat` 均返回参数错误。新显式动作继续调用原业务实现和 IPC 方法，因此功能、默认值、筛选行为及输出结构与改造前一致。

只附带全局 `--socket` 的命令组仍视为没有指定动作。只附带 `--json` 的命令组返回参数错误，stdout 为空、退出码 2，并在 stderr 提示显式操作。例如 `kproxy --json logs` 需要使用 `kproxy --json logs show`，不能在机器输出中返回帮助文本。

显式帮助请求优先，例如 `kproxy logs --json --help` 仍输出普通帮助并成功退出。`--json` 只约束业务数据输出，不把帮助、指南和补全脚本变成 JSON。TTY、重定向和管道不改变命令解析或执行含义。

发布前需要在版本说明中明确列出被移除的旧入口，以及四个裸命令和 `--json` 形式的行为变化。

## 5. 帮助页面与发现入口

总帮助按用途补充“常用查询”“命令组”“宿主机操作”“帮助与补全”索引，不改变实际命令路径。每个命令组在 Commands 清单中列出，进入组页面后展示全部直接子命令和常用示例。

总帮助覆盖所有公开顶级入口；组帮助覆盖该组所有公开直接子命令；`help --all` 递归展开所有公开命令路径及一句说明。别名附在主名称后，不重复展开整棵子树。

目标等价关系为：

```text
kproxy                    = kproxy --help = kproxy help
kproxy logs               = kproxy logs --help = kproxy help logs
kproxy logs trace --help   = kproxy help logs trace
```

`-h` 可以比 `--help` 更简短，但两者包含相同的公开子命令集合。无参组帮助与 `help <路径>` 采用长帮助格式，包含使用示例。帮助中的 Usage 必须显示完整路径，例如 `kproxy logs trace`。

`kproxy logs` 的输出原型：

```text
日志查询与排查

用法：
  kproxy logs <command> [options]

子命令：
  show      查看近期请求日志
  follow    持续跟踪请求日志
  trace     按 Trace ID 查询完整请求链路
  files     列出 daemon 日志文件
  path      查看日志目录及配置

全局选项：
  --socket <PATH>  指定管理 socket
  --json           以 JSON 输出业务数据
  -h, --help       查看帮助

常用示例：
  kproxy logs show --tail 100
  kproxy logs follow --level error
  kproxy logs trace <TRACE_ID>

使用 kproxy logs <command> --help 查看详细参数。
```

动作参数放在对应动作的帮助里，组页面优先展示能力和下一步操作；当前页面以显式动作作为唯一推荐写法。

`guide` 接收原 `HELP_TOPICS` 中的全部主题，现有说明内容迁移后校正示例。`guide` 无参列出主题及简短描述。命令帮助附“操作说明：kproxy guide <主题>”入口，命令参数列表继续从命令定义生成。

`help` 只匹配完整命令路径和已有公开别名；原理主题统一使用 `guide`。`help balance` 和 `help logs unknown` 均返回未知命令错误，`guide balance` 才显示调度原理。`help --all` 与命令路径互斥，局部帮助使用 `help <路径>`。

未知命令保留 Clap 的拼写建议，并提供当前位置的公开候选和帮助入口；缺少叶子命令必填参数时返回参数错误。`logs trace` 不能被误当成缺少动作的命令组导航。

退出码约定：导航成功和业务成功为 0；未知命令、缺少必填参数、参数冲突为 2；业务执行或连接失败为 1。显式帮助写 stdout，错误写 stderr。已有特殊的业务状态约定以原实现为准，本轮不改变其含义。

## 6. CLI 内部结构与启动流程

当前将原先集中在 `main.rs` 的导航职责拆分为：

```text
crates/kproxy/src/
  main.rs                  Cli 类型、启动编排与业务分发
  cli/
    mod.rs                 本地导航路由与正式解析入口
    help.rs                命令路径解析、帮助渲染、完整命令树
    guide.rs               原理与操作主题说明
    completions.rs         Shell 补全生成
  commands/
    account.rs             继续承载账号命令定义与业务逻辑
    runtime.rs             继续承载其他管理命令与业务逻辑
```

这是职责拆分，没有为本轮入口调整迁移全部命令类型或业务实现。`Cli::command()` 汇总出的 Clap 命令树是公开命令和参数的唯一来源；帮助、树展示、补全使用同一来源。已有派生命令类型继续参与汇总。

帮助渲染遍历已经构建的命令树，保留完整路径、继承的全局选项、顺序、说明和公开别名。可以维护显示分区及指南内容，但不能额外手写一份公开子命令清单。[Clap 提供命令树反射和帮助渲染接口](https://docs.rs/clap/latest/clap/builder/struct.Command.html#method.get_subcommands)；实施以仓库锁定版本的 API 为准。

启动分为以下步骤：

1. 保存原始 `OsString` 参数，并用同一命令定义完成前置导航解析。
2. 对纯本地导航直接输出并退出；不加载 `.env`、业务配置，也不解析管理 socket。
3. 对业务调用加载 `.env`，然后完成正式参数解析并执行现有动作。
4. 需要 RPC 的动作才进入 socket 解析和 RPC 调用；业务执行结果沿用现有格式化函数。

前置解析只用于路由，不能代替加载 `.env` 后的正式业务解析。必须保持当前参数优先级：显式 `--socket` 高于已有进程环境，已有进程环境高于 `.env`，随后才是配置和默认值。现有 `tests/dotenv.rs` 明确检查 `.env` 中的 socket 会被业务命令采用。

裸 `logs/models/tasks/diagnose` 已归入导航。解析器不改写任何旧参数；未知选项、已移除命令和缺少叶子参数的错误在配置加载与 RPC 前返回。

`logs show` 和 `follow` 仍读取现有结构化请求记录，`trace` 仍查保留的物理分片，`files/path` 保留宿主机路径映射；入口变化不改变业务功能。

命令组无参导航主动渲染帮助并返回 0；不要只添加 `arg_required_else_help` 后就假定退出码和 `--json` 行为满足约定。叶子命令的必填参数检查继续由命令解析负责。

## 7. Docker wrapper 的配套改造

“daemon 停止时仍能看帮助”同时覆盖原生 CLI 和宿主机 wrapper。原生 CLI 可直接完成；Docker wrapper 仍需要可用的 Docker 引擎和当前部署的本地镜像，这个条件在文档中明确说明。

wrapper 当前先找运行中的容器再转发命令，单改 Rust 无法满足停机帮助。建议采用同一镜像中的临时 CLI 来解决，不在 Shell 中重写命令树：

1. 生命周期执行继续走原宿主机分支；导航请求不得进入重启、停止或卸载执行路径。
2. 普通转发在选择部署时允许找到已停止的目标容器；保持 `KPROXY_DOCKER_CONTAINER` 和 `KPROXY_COMPOSE_PROJECT` 的选择规则及多部署歧义检查。
3. 目标运行中时保留正常 CLI 转发。目标已停止时，读取该容器的实际 image ID，调用镜像中的临时 CLI 导航入口。
4. 临时容器设置内部环境标记 `KPROXY_WRAPPER_LOCAL_ONLY=1` 并复用同一公开参数解析器，只允许帮助、指南、补全、版本及无参命令组导航；业务动作在配置加载前被拒绝，提示服务未运行。
5. 临时调用直接以 `/usr/local/bin/kproxy` 为 entrypoint，使用 `--rm`、`--pull=never`、`--network none`、只读根文件系统和禁用健康检查；使用原参数数组传递，不拼接或 eval 命令字符串。导航不需要业务数据挂载。

上述 Docker 参数由官方 `docker run` 接口支持；临时容器使用精确 image ID，以保持帮助与已安装 CLI 版本一致。[Docker run 文档](https://docs.docker.com/reference/cli/docker/container/run/)

环境标记不进入公开帮助和补全，也不能放行业务动作。wrapper 会先用 `help --all` 探测镜像是否支持离线导航；旧镜像不支持时明确提示重启服务或部署匹配的镜像与 wrapper。

`restart --help`、`stop --help`、`uninstall --help` 及对应 `help <路径>` 在部署环境可用时转到统一 CLI 帮助；Docker 不可用时保留宿主机生命周期命令的最小帮助作为降级说明。wrapper 会跳过位于命令前后的 `--json`、`--socket` 全局参数来识别生命周期命令，生命周期确认和执行参数解析保留原语义。

保留 `logs files/path` 的宿主机卷路径、SSO 批量文件 stdin 转发、编辑器终端环境，以及管道和 JSON 的输出边界。停止状态的临时导航调用不分配 PTY，stdout 和 stderr 分离。

没有目标容器、无法确定对应镜像、Docker 不可用或存在多个未明确选择的部署时，给出可操作错误。这些情况下不承诺完整离线帮助；安装后的 Shell 补全仍可独立使用。

`docker-setup.sh` 会在修改 Compose 资源前只读预检 wrapper 目标，提前拒绝非本项目管理的同名命令；新镜像验证通过后才原子安装匹配 wrapper。健康或回退失败时不会触碰旧 wrapper，安装失败也不会报告整次升级成功。Docker stub 测试覆盖预检、安装顺序和失败部署。

## 8. Shell 补全与文档迁移

新增静态补全生成入口：

```text
kproxy completions bash
kproxy completions zsh
kproxy completions fish
```

stdout 仅输出对应 Shell 脚本，安装说明放在命令帮助和文档中。生成内容覆盖公开子命令、选项、已有可见别名、枚举值和文件路径提示。补全使用显式 `models list`、`tasks list` 等推荐写法；安装后的 Tab 补全不连接 daemon，也不运行 Docker 查询。

本轮不增加需要联网的账号、模型或任务名动态补全。`completions` 缺少 Shell 参数时按叶子参数错误处理，`completions --help` 展示三种选择。

使用 `clap_complete = 4.5.24` 的静态生成能力，并保持现有 `clap = 4.5.16` 不变；依赖和锁文件均已更新。`generate` 从同一份 Clap 命令定义生成脚本。[clap_complete 文档](https://docs.rs/clap_complete/latest/clap_complete/)

同步修改以下文档的主示例和迁移说明：

- `README.md`、`README.zh-CN.md`：CLI 清单、模型查询、任务查询、帮助与 guide、补全安装。
- `docs/startup-and-debugging.md`、`docs/startup-and-debugging.zh-CN.md`：cargo run 示例、排障入口、停机帮助、wrapper 与镜像升级。
- `deploy/docker-setup.sh` 的安装完成提示：展示显式查询及帮助入口。
- CLI 自带 `about/after_help` 和 guide 文案：只展示当前显式写法。

迁移替换以实际命令含义为准，不机械替换全部 `logs` 字样。例如 `docker compose logs`、日志组帮助、`logs trace` 和 `alert logs` 不应被改成 `logs show`。

## 9. 实施拆分与涉及文件

下面保留原建议拆分作为评审索引；本次没有建立提交或发布。

| 次序 | 交付内容 | 主要文件 | 建议提交主题 |
| --- | --- | --- | --- |
| 1 | 提取 CLI 定义和路由；保留业务解析优先级 | `main.rs`、新增 `cli/mod.rs`，调整相关类型引用 | `refactor(cli): separate command parsing from runtime setup` |
| 2 | 增加 `models list`、`tasks list`、`diagnose all` | `cli/mod.rs`、`main.rs` | `feat(cli): add explicit model task and diagnosis actions` |
| 3 | 多级 help、guide、完整命令树和组导航 | 新增 `cli/help.rs`、`cli/guide.rs`，调整 `runtime.rs` | `feat(cli): unify help and expose command discovery` |
| 4 | bash/zsh/fish 静态补全 | 新增 `cli/completions.rs`、两级 `Cargo.toml`、`Cargo.lock` | `feat(cli): generate shell completions` |
| 5 | 停机导航、移除隐藏入口及部署一致性 | `deploy/kproxy-docker`、`deploy/docker-setup.sh`、`deploy/install-kproxy-wrapper.sh`、CLI 路由 | `fix(cli): support navigation with stopped daemon containers` |
| 6 | 中英文示例、补全和迁移说明 | 两份 README、两份启动文档、安装提示 | `docs(cli): document explicit commands and migration` |
| 7 | 切换四个无参组、移除旧参数并更新机器模式错误 | 路由、帮助、相关测试和文档 | `feat(cli)!: require explicit group actions` |

第 1 项是职责拆分，业务命令的已有行为应保持稳定；显式入口、导航和旧语法移除可按表中顺序拆分提交。发布说明需要列出 `BREAKING CHANGE`。

## 10. 验证与验收

本方案新增的是命令契约和转发行为，需要验证它们的实际外部表现。保留现有业务测试，增加进程级 CLI 测试与 Docker stub 测试；不依赖真实账号、真实推理或发送通知来验证入口。

| 验证对象 | 关键用例 | 通过条件 |
| --- | --- | --- |
| 公开命令覆盖 | 总帮助、十个命令组帮助、`help --all`、补全 | 公开入口全部可达；已移除参数和内部环境标记不混入公开列表 |
| 帮助一致性 | `--help` 与 `help <完整路径>`，含嵌套路径和可见别名 | 相同路径、参数及子命令集合；完整 Usage 正确 |
| 参数错误 | `logs unknown`、`logs trace`、`help logs unknown`、`logs --tail 10 files` | 退出码 2；错误在 stderr；无业务调用 |
| 新显式动作 | `models list`、`tasks list`、`diagnose all` | 与原对应动作复用相同 RPC；刷新顺序、诊断默认值一致 |
| 无参切换 | 四个裸命令及只有 `--socket` 的形式 | 输出组帮助，退出码 0；没有 RPC、配置加载或探测 |
| 机器模式 | `--json logs/models/tasks/diagnose` | 退出码 2，stdout 为空，提示显式动作 |
| 旧语法移除 | 旧 logs/models/alert 参数、`add-sso-batch`、`help balance` | 退出码 2；配置加载与 RPC 前失败 |
| 显式帮助优先 | `logs --json --help`、`uninstall --help` | 正常帮助；不触发查询或生命周期执行 |
| 原生独立导航 | 缺少 socket、损坏业务配置、格式错误的 `.env`、只读工作目录 | 帮助、guide、补全和版本仍可用；不产生数据文件 |
| 环境优先级 | CLI、进程环境、`.env` 中分别设置 socket | 显式参数优先；已有环境优先于 `.env`；原 dotenv 测试继续通过 |
| 指南主题 | `guide balance`、`guide sso`、`guide docker`、`guide logs` | 正确显示指南；`help` 只接受命令路径 |
| 补全脚本 | 三种 Shell 生成、主要子命令与选项、语法检查 | stdout 仅含可加载脚本；不含隐藏入口；加载后补全可用 |
| Docker 停机帮助 | 运行、停止、无容器、多部署、旧镜像场景 | 选中正确版本；停止场景只运行导航 CLI；错误明确 |
| 内部入口限制 | 对临时容器设置 `KPROXY_WRAPPER_LOCAL_ONLY=1` 后调用业务动作 | 配置加载前拒绝业务；没有数据挂载、服务启动或真实业务调用 |
| wrapper 回归 | JSON/管道、日志宿主机路径、SSO stdin、编辑器、生命周期帮助 | 原转发能力保持；帮助不触发业务或生命周期动作 |
| 部署升级回退 | 镜像失败、健康失败、wrapper 安装失败、成功升级 | 失败如实报告，旧 wrapper 保持可用，成功后两者一致 |

已新增 `crates/kproxy/tests/cli_help.rs`、`docker_wrapper.rs`、`wrapper_installer.rs`，扩展 `tests/docker_setup.rs`，并在 `main.rs` 和进程测试中覆盖旧语法拒绝行为。

RPC 等价性使用隔离的 Unix socket 测试服务记录收到的方法和参数，并返回固定结果；对导航和旧语法错误断言没有收到请求。帮助测试固定非终端输出，核心断言检查命令可达性、退出码与副作用，避免仅锁定排版。

实施后的基础检查：

```text
cargo fmt --all -- --check
cargo test -p kproxy --locked
sh -n deploy/kproxy-docker deploy/install-kproxy-wrapper.sh deploy/docker-setup.sh
```

由于 CLI 类型仍引用 workspace 内部 crate，发布前再做一次 workspace 测试和构建检查；如依赖变化影响 Docker 构建，沿用 Dockerfile 的 slim/full 构建组合做本地验证。上述检查不包含推送镜像、打 tag 或部署。当前 tag 工作流会发布镜像，不能用发布流程替代普通验证。

验收完成的标志是：用户输入任意命令组即可看到下一步操作，数据查询都用显式动作表达，旧参数在业务初始化前被拒绝，停止业务服务后仍能查看同版本导航。

## 11. 本次交付状态

已完成显式业务动作、无参组帮助、多级 help、guide、完整命令树、三种 Shell 补全、旧参数移除、本地导航启动顺序、停止容器导航、wrapper 原子升级及中英文文档迁移。

已增加进程级 CLI、RPC、Docker wrapper、部署顺序和 wrapper 原子替换测试。复查阶段修正了旧参数泄漏到补全、诊断目标校验过晚、生命周期帮助组合被 wrapper 提前拒绝、生命周期命令无法识别前置全局参数、wrapper 目标冲突发现过晚，以及临时文件名可预测的问题；随后按最终要求彻底删除旧命令写法。2026-09-08 已通过格式检查、workspace Clippy、workspace 全量测试、Shell 语法检查、Bash/Zsh 补全语法检查和 `docker compose config --quiet`；全量测试中一个依赖本地 Claude Code 的既有用例按原条件忽略。
