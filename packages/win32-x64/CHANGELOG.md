# @wecom/cli-win32-x64

## 1.3.0

### Minor Changes

- 6967dc6: 收紧文件读写沙箱
  - CLI 此前不注入任何沙箱根、可读写任意位置；现在读写均限制在当前工作目录与系统临时目录内。凭据目录与配置目录（含任意基座下的 `**/.config/wecom`）一律拒绝。
  - 内置 deny 清单覆盖凭据形状路径（`.ssh`、`.aws`、`.kube`、`.env`、`.git`、`*.pem`、`*.key`、`id_rsa` 等）与系统目录，即使位于当前工作目录内同样拒绝。
  - 迁移指引：如需读写当前工作目录之外的文件，请在目标目录下运行 CLI，或使用相对当前工作目录的路径。`--output` / `--output-dir` 仍控制文件落点，但目标必须落在上述范围内。
  - 错误码与错误输出格式不变（CLI 段 893000–893299；后台 errcode 原样透传）。
- 6967dc6: 重构下载落盘位置与 `--output-dir` 语义
  - 下载产生的文件（运行时二进制响应、响应内容导出为文件）默认落盘到当前工作目录，不再是 `<tmp_dir>/requests`——对齐 wget / curl `-O` / `gh release download` 的默认行为。
  - `--output-dir` 只对产生文件的响应生效；不再把 JSON 响应落盘为 `<method>.json`（分页时为 `<method>.ndjson`），纯 JSON 响应不产生文件。保存 JSON 响应请改用 `--output/-o` 或 shell 重定向。
  - 显式声明的 `--output-dir` 一律经沙箱校验，目标必须落在当前工作目录或系统临时目录内。
  - 移除 `tmp_dir` 配置（`WECOM_CLI_TMP_DIR` 环境变量、`config.json` 的 `tmp_dir` 字段及对应 builder API）与派生的 `request_storage_dir`。嵌入方可经 `ClientBuilder::default_output_dir` 设置客户端级默认下载目录（默认当前工作目录；单次调用仍可用 `--output-dir` 覆盖）。
- 6967dc6: 移除文件路径模糊纠错
  - 路径不存在或输入有误时，不再自动纠正为名称相近的真实文件；统一以「找不到目标文件」报错，不会按纠正后的路径静默继续。
- 6967dc6: 精简 JSON 自动修复提示，避免载荷外泄
  - `--json` / `--set` 输入被自动修复时，stderr 提示精简为单行，不再回显修复前后的 JSON 原文。
  - 遥测事件不再携带修复前后的载荷内容，仅保留修复策略信息，避免敏感数据外泄。

### Patch Changes

- 6967dc6: 外部路径相对值统一锚定到进程工作目录
  - `WECOM_CLI_CONFIG_DIR` 的相对值按进程工作目录解析为绝对路径；空值按未设置处理，回退默认配置目录。

## 1.2.1

### Patch Changes

- 56cd4c5: 新增编译特性 call-chain

## 1.2.0

## 1.1.0

## 0.1.9

### Patch Changes

- 3700b1b: update cmds

## 0.1.8

### Patch Changes

- 7774ba3: update init process

## 0.1.7
