<h1 style="font-size: 48px;">别倒卖了😭</h1>

# 格陵兰岛

> [!CAUTION]
> **仅限学习研究，禁止任何实际使用。**
>
> 本项目是协议分析与软件工程的研究样本，**不是用来打卡的工具**。作者不提供、也从未提供
> 代跑 / 代打卡 / 代做体育考核的任何服务。使用者因使用本项目产生的一切后果（学校处分、
> 体育成绩作废、账号封禁、行政处罚、民事赔偿、刑事追诉等）**由使用者本人承担，作者不承担
> 任何责任**。使用即表示你已独立判断所在学校规定与当地法律允许你这样做。
>
> 完整条款见文末 **[免责声明 / DISCLAIMER](#免责声明--disclaimer)**。

![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue)

运动世界校园版全协议自动化工具，纯 Rust 单文件，GUI / CLI 双模式。

## 关于本项目

**格陵兰岛（Greenland）是 [NekoSportsWorldTool](https://github.com/YanamiNeko/NekoSportsWorldTool) 的二次开发版本。**

| 项 | 内容 |
|---|---|
| 原作者 | YanamiNeko |
| 原项目 | https://github.com/YanamiNeko/NekoSportsWorldTool |
| 许可证 | [CC BY-NC 4.0](LICENSE)（署名—非商业性使用 4.0 国际） |

本项目保留原作者的完整署名，并按同一许可证发布：

- 转载、分发、二次修改必须注明原作者 **YanamiNeko** 及原项目地址；
- **禁止任何商业用途**（包括出售、收费代做、广告变现、集成进商业产品）；
- 本仓库相对原版所做的修改，同样适用上述两条。

## 相对原版的改动

> 原版的轨迹生成器是「打卡点拟合环 + 自然速度曲线 + 哨兵点 / 断崖 / 点位吸附」（见原版 README）。
> 本版在此基础上重做了路线引擎，并补齐校园数据与批量运营能力。

- **操场轨迹真实化**：按标准 400m 跑道几何生成（直道 84.39m + 半圆 R36.5m），支持 1–8 道分道、圈间换道、叠加 GPS 定位噪声；5 个打卡点用圆锥曲线拟合真椭圆，首点严格等于 P1，2km 目标不再多跑一整圈。
- **校园路网模式**：GeoJSON 导入区域 / 禁行区 / 连廊 / POI / 路网；A\* 避建筑、重复路段罚分（回程不原路重叠）、净空档位自动降级（10m → 6m → 3m）、边界内缩。
- **海拔拟真**：按大区估基准海拔，支持固定值或区间映射（保留相对起伏），叠加高频抖动让相邻点海拔差接近真人（锯齿中位 ≈1m），并计算累计上升 / 下降。
- **校园数据管理**：`campus.json` 数据模型、GeoJSON 导入校验、版本快照（改动可回滚）。
- **批量账号运营**：账号导入 / 批量登录 / 批量刷跑，备注、已购公里、专属代理、按账号覆盖设备平台，配套违规自查与学期完成度。
- **移除自动更新**：本版不接自更新（桌面端手动替换 exe、Android 手动安装 APK），避免更新源与签名绑定。
- **开源化整理**：清除仓库内的真实校园与账号数据，示例几何改为合成坐标，测试坐标改为中立占位值，多余开发产物不随仓库发布。

## 功能

- **跑步**：一键全链（策略 → 打卡点 → 轨迹 → 提交 → OBS → 验证），轨迹按打卡点自然生成，开始时间支持随机或指定到前 3 天
- **AI 运动**：任务 / 自由练习双模式，批量补签（前 60 天 × 多项目）
- **数据**：学期完成度、违规自查、排行榜（个人/班级/院系 × 日/月、室内榜、历史榜）、个人主页
- **登录**：支持人机验证自动通过
- **双端**：桌面端（Windows / macOS / Linux）与 Android，共用同一套协议层

## 界面

![GUI](docs/screenshot.png)

## 技术特点

- 完整实现 NetSecKit 信封加密与响应解密，每个响应强制 RSA 验签（fail-closed）
- 人机验证全程本地完成，不依赖任何外部服务
- 轨迹生成器：操场走**标准跑道几何**（直道 84.39m + 半圆 R36.5m，1–8 道分道、圈间换道、GPS 噪声）；校园通路网 A\*（避建筑 + 重复罚分 + 净空降级）
- 设备身份持久化（DeviceId / 安装时间 / MAC），稳定不易触发风控
- GUI 与 CLI 共用同一套协议层，数据文件互通

## 使用

双击 `greenland.exe` 进入 GUI；CLI 用法（Linux / macOS 下二进制名为 `greenland`）：

```text
greenland login --user <手机号> --pass <密码> --remember
greenland run                                  # 一键跑步（--altitude 17.2 固定海拔，或 11.6-22.8 映射到区间）
greenland ai --sport 2 --score 26000           # AI 运动
greenland semester                             # 学期完成度
greenland cheat                                # 违规自查
greenland rank main --type 1 --sort 1          # 排行榜
greenland template --file run.gpx              # 本地分析真实记录海拔（不会上传）
greenland help                                 # 全部命令
```

`template` 只读取用户手动选择的本地 GPX/JSON 文件，输出采样点、海拔范围、累计上升和累计下降，
不会登录、访问服务器或把模板记录接入跑步上传流程。

手动海拔支持三种状态：留空使用自动海拔；填写单个数字（例如 `17.2`）让所有轨迹点使用固定海拔；填写区间（例如 `11.6-22.8`）将生成的海拔曲线映射到该上下限内，同时保留轨迹的相对起伏。

青龙定时任务：首次手工 `login --remember` 一次，之后定时挂 `run` 即可。

## 首次使用

首次启动会在数据目录生成 `identity.json`，其中的城市与定位锚点是**固定占位值**（`示例市` + 一个与你的实际位置无关的占位坐标），
必须在「设备信息」页改成你实际跑步的城市与锚点，否则程序会拒绝提交（避免把占位坐标发给服务端）。

从改名前的旧版升级：旧数据目录会自动搬迁到新目录（只复制、不覆盖）；存放在自定义位置的旧数据，
可用环境变量 `ESVIM_LEGACY_DATA_DIR` 指定，启动时同样自动复制过来。

## 常见问题

- **提示未登录 / 401**：先执行 `login --remember`（GUI 勾选记住密码等价），会话失效会自动重登
- **10121 设备风险**：设备身份是持久化的，删除 `identity.json` 等于换了新设备，不要频繁删
- **10603 点位限流**：点位接口 5 分钟限 3 次，程序内置 300s 缓存，正常使用不会触发
- **榜单为空**：当天还没有人产生有效里程，查询会自动回退最近 3 天
- **升级方式**：本版不接自动更新。Windows 下载新的 `greenland.exe` 后退出软件，用新文件替换旧文件；Android 下载新 APK 覆盖安装
- **配置文件**：请保留 `config.json`、`identity.json`、`points_cache.json`、`campus.json` 等文件，不要删除或修改
- **人机验证失败**：自动重试 3 轮，仍失败大概率是网络波动，稍后再试
- **杀软误报**：未签名编译产物可能被误报，自行判断后加白

## 构建

```bash
cargo build --release                        # GUI + CLI
cargo build --release --no-default-features  # 仅 CLI
```

推 tag 自动构建 Windows / macOS / Linux 四平台产物并发布 Release（见 `.github/workflows/release.yml`）。

## 测试

```bash
cargo test                        # 单元测试
cargo run --example flow_dryrun   # 离线干跑「操场 / 校园」两条路线分支，不登录不联网
```

道路 V3 的本地验证需要自备 `campus.json`，验证点通过环境变量传入（须落在校园范围内）：

```bash
ESVIM_V3_POINTS="lng,lat;lng,lat;lng,lat;lng,lat;lng,lat" cargo test --test v3_road -- --nocapture
```

## 文档

- [路线生成 v2 企划书](docs/路线生成v2-企划书.md)
- [路线生成 v2 实施计划书](docs/路线生成v2-实施计划书.md)

## 许可证

[CC BY-NC 4.0](LICENSE) © YanamiNeko（禁止商用）

本项目是 [NekoSportsWorldTool](https://github.com/YanamiNeko/NekoSportsWorldTool) 的二次开发版本，
署名与原项目地址按许可证要求保留；任何分发或修改都不得移除上述署名，也不得用于商业用途。

---

## 免责声明 / DISCLAIMER

**本项目不是供人使用的工具，而是一份协议分析与工程实践的研究样本。**

### 一、性质声明

1. 本项目仅供**个人学习、协议研究、逆向工程与软件工程实践**之用，不得用于任何实际场景。
2. 本项目与运动世界校园版（及任何学校、教育机构、运营方）**无任何隶属、合作、授权或背书关系**。
3. 作者从未、也不会以任何形式提供代跑、代打卡、规避体育考核或任何作弊服务；
   本仓库不包含任何账号、成绩或代做交易相关内容。
4. 作者未以任何方式鼓励、教唆或协助他人违反所在学校的规章制度或学术诚信要求。

### 二、禁止用途

严禁将本项目（含任何衍生版本、修改版本或部分代码）用于：

- 替代或伪造真实体育锻炼记录，规避学校体育考核；
- 代他人打卡、批量代跑、出售成绩，或任何形式的作弊服务；
- 违反所在学校学生手册、学术诚信规定、体育课程纪律的任何行为；
- 违反所在地法律、行政法规、部门规章或平台用户协议的任何行为；
- 将本项目集成进任何商业产品或用于任何商业目的。

### 三、责任免除

在适用法律允许的最大范围内：

1. 本项目按「**现状**」（AS IS）提供，**不附带任何明示或默示的担保**，包括但不限于适销性、
   特定用途适用性、不侵权、无错误、不中断、结果准确的任何担保。
2. 作者**不对因使用或无法使用本项目而产生的任何直接、间接、附带、特殊、惩罚性或后果性
   损失承担责任**，包括但不限于：
   - 学校处分（警告、记过、取消体育成绩、取消评优或推免资格、留校察看、开除学籍等）；
   - 账号封禁、成绩作废、身份信息被标记或列入黑名单；
   - 数据丢失、设备损坏、时间损失、第三方索赔；
   - 任何民事赔偿责任或行政处罚；
   - 任何刑事追诉风险。
3. **使用者自行承担使用本项目产生的全部后果**。是否运行、如何使用，完全出于使用者本人的
   自主决定与独立判断；作者没有也无意施加任何诱导。
4. 作者**不承担任何形式的咨询、维护、答疑、适配或技术支持义务**，也没有义务保证本项目在
   任何时间、任何版本的应用或服务器上可用或有效。
5. 使用者应当**自行核对其行为是否符合所在学校规定与所在地法律**。若该类软件的分发或使用
   在当地被禁止，**你应当立即停止使用并删除本项目的全部副本**。
6. 本节内容与 [CC BY-NC 4.0](https://creativecommons.org/licenses/by-nc/4.0/legalcode) 第 5 条
   （免责声明与责任限制）共同适用；两者冲突时，以对作者更有利的条款为准。

### 四、知悉确认

下载、编译、运行、分发或修改本项目，即表示你已完整阅读、理解并同意本声明，
且已**独立判断**你所在学校的规定与所在地法律允许你这样做。
若你不接受上述任何一条，请立即停止使用并删除本项目的全部副本。

---

**Disclaimer (English)**

This project is provided **strictly for educational, protocol-analysis and software-engineering
research purposes**. It is **NOT** intended for actual use, and it is **NOT** affiliated with,
authorized by, or endorsed by any school, educational institution, or service operator.
The author has never offered, and does not offer, any service that runs, fakes, or replaces
physical exercise records on anyone's behalf.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

**You are solely responsible for any and all consequences of using this software**, including
but not limited to academic penalties, disqualification of grades, account bans, loss of data,
civil liability, administrative penalties and criminal liability. You are responsible for
verifying that your use complies with your institution's rules and the law of your jurisdiction.
If such use is prohibited, you must cease use immediately and delete all copies.
By downloading, building, running, distributing or modifying this software, you acknowledge that
you have read, understood and accepted this disclaimer, and that you accept all risks yourself.
