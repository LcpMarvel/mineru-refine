# TODO — 基于 test_data 三份文档精修结果的残留问题分析

> 数据来源：`test_data/mineru/`（原始）vs `test_data/refined/`（精修后），2026-06-12 分析。
> 三份文档：MN-JZY-001 战略管理规范（71 页）、MN-ZBZ-003 管理评审程序（9 页）、
> MN-ZBZ-047 组织绩效管理规范（17 页）。
>
> 已完成（2026-06-12，core 0.9.0 / confusion c3）：
> ① 赘字/衍字删除（extra_char 探测器 + deleteChar op，全走 LLM 裁决，无机械兜底）；
> ② confusionObservations 闭环回灌（一轮二次候选，三道闸门照旧）；
> ③ 中文形近字准入名单扩充（校较/酒源/军率）+ 全文频率投票（加白排误 + 拉丁 token 少数派投票）。
>
> 已完成（2026-06-12，core 0.10.0 / garbled g1）：
> ④ 重度乱码表视觉重转写层（`rewriteGarbledTables`，opt-in，见下「1.」原案与实现差异）。
>
> 已完成（2026-06-12，core 0.12.0 / p7，第二轮复查产出，详见「5.」）：
> ⑥ token 频率投票机械落地（`mech:token_vote`，默认开）；
> ⑦ 乱码表降级兜底（`degradeGarbledTables`，opt-in）；
> ⑧ caption_heading 探测器 + extractCaption op（被吞进表格题注的小节标题）。
>
> 已完成（2026-06-13，VL 确定性）：
> ⑨ Qwen-VL 视觉裁决加 `top_k:1` 贪婪解码——根因是 DashScope 的 temperature 有效区间
>    是【开区间 (0,2)】，传 0 落区间外被静默忽略、回落默认采样（temp0.8/top_p0.8），这才是
>    §4 记录的「mergeTable 同输入跑出 100/102 items」漂移真因。`top_k:1` 把 softmax 退化成
>    确定性 argmax（温度从此不起作用）。两处 VL 调用（judge_split_table/transcribe_table）
>    都带上。回归验证（/refine-regression 真 LLM）：003/047 旧码→新码逐字节无差异（二者
>    各跑 mergeTable×2/×1），JZY mergeTable×7 两次一致——VL 侧零漂移。详见「4.」。
>
> 已完成（2026-06-12，bugfix）：
> ⑤ 图注/图脚注静默丢失修复——`render_markdown`（markdown.rs）与 caption 探测器
>    （detect.rs）查的字段名是 `img_caption`/`img_footnote`，但 MinerU 真实字段是
>    `image_caption`/`image_footnote`，永远取到 null。后果：重渲染的 full.md **静默丢掉
>    全部图注**（JZY-001 实测 17→2）；caption_issue 对图片误判。`markdown_test.rs` 的
>    fixture 也写成 `img_caption`，与代码"错得一致"互相掩护，所以测试一直绿。已三处一并
>    改回真实字段名，重跑后 8 张图图注全部正确渲染、caption_issue 41→33。

## 1. 重度乱码表格的视觉重转写 ✅（core 0.10.0，`rewriteGarbledTables`）

实现为独立 opt-in 层（`garbled.rs`），跑在出口闸门后、混淆层前：

- [x] 机械检测：内嵌 6 万常用词词典（jieba top60k，`data/cn_words.txt`），对单元格汉字段
      做正向最大匹配算词覆盖率。标定（三份真实文档）：乱码表 0.46，最差正常表 0.61，
      阈值 0.55，全语料零误报。原案的「observation 密集」信号不需要——覆盖率单信号已分干净，
      且让本层与混淆层解耦
- [x] 复用 `imageDir`/`LoadImage` + Qwen-VL（`VisionClient::transcribe_table`），
      对照 img_path 截图逐单元格重转写
- [x] 三道机械闸门 + 全量留痕：①资格（空格/纯数值/短编号/词覆盖率正常的格不许动——
      实测视觉模型在 33 列宽表上会行列错位张冠李戴，这道闸门拦下全部错位提案）；
      ②结构（行列命中/无标签/长度上限/提案非纯数值/长度量级可比）；
      ③整表回归（重转写后覆盖率必须严格升高，否则整表回退）。
      每格进 `report.tableRewrites`（before 即撤销凭据）与 provenance（origin=garbled_table）
- [x] Midhuel→Michael 词级错误顺带解决（实测 ZBZ-047：25 格落地，覆盖率 0.46→0.77，
      含 Michael×2；冒烟入口 `cargo run --example garbled_smoke --features bin`）

## 2. 单元格内 LaTeX 残留 ✅（机械清洗 pass 第 6 件：`mech:cell_latex`）

`strip:latex_dollar` 只扫**独立文本** item，单元格内的 `$...$` 不碰。JZY-001 精修后
表格里仍躺着 `$\lambda _ { m i }$`、`$\lambda _ { m a x }$` 这类被 MinerU 误转的 LaTeX
片段——默认配置（两个 opt-in 层都关）下肉眼可见的脏点，且不依赖 LLM/VL，纯机械可清。

与原案差异：没走 strip op 扩展——cell 受「table_body 逐字节不变」保真闸约束，op 体系
动不了；落在机械清洗 pass（mechanical.rs，基线快照前），这也正好兑现"纯机械可清"。

- [x] cell 内 `$...$` 伪 LaTeX 包装剥除：已知符号命令换 Unicode（`\lambda`→`λ`、
      `\times`→`×`，原 strip 直接丢命令会把 λ 丢掉），样式包装命令（\mathsf 等）删除，
      花括号/公式内空白删除
- [x] 真公式不动：命中任何未知命令（\frac/\sum/\begin…）整段保留；裸 `$100$`
      （无命令无花括号）不动——可能是真美元金额；`\$` 转义定界符不碰（先于 unescape 跑）
- [x] 自校验 + 留痕：span 级字符预算校验（产物字符 ⊆ 原 span 字符 + 命令映射符号）+
      表级期望值严格比对，不过即整表回退；原 span 进 removedSpans
      （reason=`mech:cell_latex→替换文`，即撤销凭据）
- [x] 实测三份文档：JZY-001 恰命中两格 → `λ_mi`/`λ_max`，其余零误报零回退

## 3. 跨页段落粘连 / 漏标标题残留（探测器调优）✅（missed_heading 子项信号）

JZY-001 精修后仍有约 16 处疑似跨页断句，抽查分两类：

- 真断句：item471「--收割战 ‖ 防御战的核心」
- 漏 promote 的小节标题：「战略管理之"看趋势"」以 text 身份反复出现在页首

复盘结论（对照 origin.pdf 逐处复核）与原案有出入：

- **「真断句」抽查系误判，merge 侧不需要调**。item471「---收割战」是 p39 五连
  项目符列表（---防御战/---正面战/---游击战/---侧翼战/---收割战，OCR 吃掉部分短横）
  的末项，下一页「防御战的核心」是图示小标题——merge 了反而错。宽松口径重扫精修后
  全部 13 处跨页候选，全是「列表项相邻」「冒号引出列表/图表」形态，探测器静默全部正确。
- **「战略管理之看趋势」页眉泄漏已被家具同文佐证 drop 解决**（本轮重跑后无残留），
  不是 promote 缺口。
- **真正的召回缺口**：整组编号兄弟都被 MinerU 漏标时（「2.1确定竞争对手：」
  「2.2竞争情报收集：」…），原有「±1 相邻兄弟是标题」信号永不触发——
  JZY-001 原始数据 15 处全因此漏网，规模恰对上「约 16」。

- [x] missed_heading 新增**子项证据**信号：下一个内容块的编号以本块编号为真前缀
      （同数制，如 2.1 → 2.1.1）即标。表面闸照旧（≤30 内容字、无逗号/句末标点）。
      实测：JZY-001 命中 13 处（其余 2 处由兄弟信号互补覆盖），另两份文档零新增命中
- [x] 回归用例落地 detect_test.rs：正例（同页子项、跨页隔家具子项）+ 负例
      （非前缀编号、数制不匹配、收割战列表尾、冒号引出去符列表）
- [x] 端到端重跑 JZY-001：promote 9→18，「2.1确定竞争对手：」「2.2竞争情报收集：」
      「1.1定期评估」等抽查 11 处全部落地 level=2，子项形态残留清零，violations 0

## 4. 观察（已知，暂不单独立项）

- **mergeTable 合表 ≠ 修内容**：ZBZ-003 序号 1-10 输入信息表跨页合并成功，但续表本身
  OCR 烂——`3）亻`（拆字）、`2 3)`（并格错位）、`有效 性`（空格断字）原样保留。合表只接
  结构，单元格内乱码归 `rewriteGarbledTables`（opt-in）/「2.」管，默认配置不治。
- ~~**跨运行非确定性**：mergeTable 走 Qwen-VL 视觉裁决，同一输入两次跑出 100 / 102 items
  不同结果~~ ✅（2026-06-13，见上⑨）：根因不是「VL 本质非确定」，而是 `temperature:0` 在
  DashScope **开区间 (0,2)** 外被静默忽略 → 回落默认采样。加 `top_k:1` 贪婪解码后钉死。
  回归实测：003（mergeTable×2，正是当年 100/102 漂移的那份）旧码→新码逐字节无差异。
  **走降温/贪婪解决，未加缓存。**
- **文本侧（DeepSeek）抖动仍在**（与上条独立）：DeepSeek 走 tool-call 路径，虽 temperature
  已 0，但偶发 `llm_no_tool_call` / 轮次耗尽会让某疑点这轮落、下轮不落。JZY 真 LLM 两跑
  实测 4 处差异（2 改进：多 drop 一页眉、demote 一伪标题；2 回归：衍字「的的」未删、
  「附件3/编制人」caption 残留回潮）。`top_k` 治不到（病灶是模型不调工具，非采样温度）。
  待议——若要稳，方向是 tool_choice=required 兜底 / 搁置项二次重试，不属本次范围。

## 5. 第二轮复查（2026-06-12，通读三份 refined full.md 全文）

> 注：复查所用 test_data/refined 产物是【默认配置】跑的（report 无 confusionFixes/
> tableRewrites），opt-in 层的效果未体现。下列「混淆层已覆盖」指机制已有，默认配置不生效。

### 5.1 拉丁 token 频率投票的机械落地（0/O 等形近混淆）✅（core 0.12.0）

实测残留：`SW0T`×6（JZY full.md 多处）、`0GSMT`、`S/W/0/T`、`CE0`×3（047）、
`0A系统`、SWOT 表里 O1~O12 全写成 `01`~`012`、003 版本号 `la`→`Ia`。

- 混淆层（opt-in）机制上已覆盖大半：0/O 在内置等价类里、prompt 点名 CE0/0A 例子、
  SW0T 这类还有频率投票注记直通。但默认配置（层关）零修复，且每个 0/O 都送审烧 token。
- [x] 机械清洗 pass 第 7 件 `mech:token_vote`：全文 latin token 频率统计（≥4 字、
      ≤1 数字，与混淆层同口径），少数派 token 与多数派（≥4 次且 ≥3× 少数派）恰差一处
      单字替换、且 (before,after) 同属内置混淆等价类 → 机械直接落地，零 LLM。
      text/list_items/table_caption/footnote/cell 全覆盖（cell 内紧跟 &/# 的 run 不算
      token，防 HTML 实体）；校验走「替换对独立结构校验 + 代入旧值得期望值再严格比对」，
      removedSpans reason=`mech:token_vote→正写`。换位（OGSTM↔OGSMT）故意不收——归混淆层。
- [x] 实测（offline_audit，零 LLM）：JZY 落地 8 处（SW0T→SWOT×7 + 0GSMT→OGSMT×1），
      003/047 零误报。注意这是默认配置下首个"换字符"项，主 README「绝不新增一个字」
      承诺已补注此例外（证据全文自明 + 全量留痕）。
- 短 token / 无全文多数派的（CE0、0A、S/W/0/T、01、la）证据不足以机械化，
  仍归 opt-in 混淆层 LLM 裁决——不降低落地门槛。

### 5.2 乱码表「重转写救不回」时降级为图片 ✅（core 0.12.0，`degradeGarbledTables`）

重转写层（g1）是尽力而为：视觉故障搁置、闸门全拒、覆盖率回归不过都会让乱码表
原样留在产物里——一张满是「目择值/数据来酒」的假表对下游 RAG 是主动误导，
而它的 img_path 截图完全清晰。

- [x] 新 opt-in 层 `degrade_garbled_tables`（纯机械，不依赖 VL，可独立于重转写层开）：
      跑在重转写层之后——仍判废（词典覆盖率 < 0.55）且有 img_path 的表，
      整项降级为 image（table_caption/footnote 改挂 image_*，table_body 删除并进
      removedSpans 留痕 + report.tableDegraded 计数）。full.md 里呈现为图片引用。
      降级版本号 d1 进缓存 key；CLI/server/js/python/refine_real（REFINE_DEGRADE_GARBLED=1）
      五个面全部接线。
- 顺序语义：重转写层先救，救回（覆盖率过阈值）的不降级；两层都开 = 「先救、救不回再降」
  （garbled_test 金样本验证：重转写救回 → tableDegraded=0；只开降级层 → 表变 image）。

### 5.3 被吞进 table_caption 的小节标题（promote「不一致」的真身）✅（core 0.12.0 / p7）

复查时以为是 promote 漏（4.6/4.7 没升级而 4.4/4.5/4.8/4.9 都是 ##），对照源数据
发现根因完全不同：MinerU 把「报告评分表」「4.6核心组织绩效的应用」塞进了评分表的
table_caption 数组、「4.7公司十大核心指标」塞进了十大指标表的 caption，JZY 的
「更改情况」同样被吞进更改表 caption。渲染成 caption 行后看起来像漏 promote，
实际是结构错位——还顺带制造了「4.6 标题隔在『报告评分表』题注和表格之间」的错序。

- [x] 新探测器 `caption_heading`：table_caption 条目过标题表面闸（≤30 内容字、
      无逗号/句末标点）且行首编号可解析、且存在同数制同深度同父编号的相邻（±1）
      标题兄弟 → 标疑点（每表只标首个命中条目，loop 迭代收敛）。证据带
      captionIndex/兄弟 level/position 判断指引。
- [x] 新 op `extractCaption(id, captionIndex, position, level?)`：把 caption 条目
      抽出为独立 text 块（level 给则设 text_level），插在表格前/后（position 由 LLM
      按内容归属判断：表格属于标题前的小节 → after，表格是该小节首个内容 → before）。
      字符多重集不变（纯移动），table_body 不动，bbox/page_idx 继承表格，表格继承原 ID。
      工具/system prompt/op_hint 全套接线，PROMPT_VERSION p6→p7。
- [x] 实测（offline_audit）：047 精确命中 it_0197「4.6核心组织绩效的应用」与
      it_0199「4.7公司十大核心指标」两处真实病例（兄弟锚点 4.5/4.8），
      JZY/003 零误报；「表3.1 差距分析模板」类真题注因「表」前缀编号不解析为节编号而免疫。
- 无编号的被吞标题（JZY「更改情况」）没有可靠机械信号（单条目、无兄弟可佐证），
  暂不标——渲染为普通段落可接受，不值得为它放宽闸门。

### 5.4 其余残留（待办，未排期）

- ~~**跨页续表碎片**~~ ✅（2026-06-13，调查澄清——无需改代码）：原顾虑「VL 判 merge 后 ops 层
  并列错位会不会更糟」已证伪。**合并本就在跑且已落地**：迭代循环先 drop 掉空壳表（it100/p7）和
  泄漏成 text 的跑马灯页眉（it104/p8「真诺测量仪表…版本 K-」），随后主表（it96/p6，3 列）与碎片
  （it105/p8，4 列）结构相邻 → split_table 触发 → VL 判 merge → 主表 10 行长到 14 行落地
  （refined report `mergeTable:2`，`violations:0`）。**错位只在尾部 4 行**（序号9 续行 + 序号10，
  因续表页被 OCR 把"输入信息"列重切成"子编号+内容"两列，行内列数 4/4/2/2），**前 9 行干净 3 列零损**。
  对照「不合并」的替代（干净主表 + 末尾孤立乱码碎片表），合并反而把序号10 留在了它的表里、内容
  更完整——**不会更糟，反而更好**。
  - **机械无法安全修齐**：试过「后块列数 > 前块就拒合」等闸，全被已固化的合法用例反例否决——
    `ops_test::ragged_merge_2col_plus_3col_no_padding_invented`（A 尾空列被 MinerU 略去看着像 2 列、
    B 带全 3 列，B 列数 > A 但合并正确）+ `rowspan_carryover_with_unequal_columns_merges`（rowspan
    跨页携带让 A 内部参差且合法）。列数信号区分不了「OCR 重切列(该拒)」与「尾空列略去/rowspan携带
    (该合)」。唯一能真正修齐的是重型 VL 跨行跨页重折叠，且要打破 mergeTable 逐字节保真闸，性价比极低。
  - 项目哲学明确「绝不发明空单元格去对齐」（`ops_test::padded_repair_attempt_fails_row_level_gate`），
    保留即正确。已加回归 `ops_test::cross_page_fragment_resplit_columns_merges_byte_faithful`
    用 003 真实碎片 body 锁定「干净行零损 + 参差行原样 + 不发明空格 + 过行级保真闸」当前行为。
  - 碎片内乱码（`3）亻`/`2 3)`/`有效 性`）是另一回事，归 opt-in 的 `rewriteGarbledTables`/
    `degradeGarbledTables`，不属本项。
- ~~**同表双 OCR 副本**~~ ❌（2026-06-13，调查后明确排除，见「不做」清单）：渲染 origin.pdf
  p45 后真相与原诊断相反——**不是 OCR 把同一表拍两遍，而是源文档作者主动画了两份**
  （上：带深蓝标题栏「竞争分析表-通过学习和打击抢夺机会」的品牌样式展示版 it_0586；
  下：待填写的大号纯网格版 it_0587，OCR 表头带噪声且把后文「2、竞争分析表的 SWOT 输出」
  抓成 footnote）。两份都是同一张空模板。删一份属"替作者改稿"，碰保真红线。
- **公式碎裂进多格**（JZY 判断矩阵表尾 `一-致性检验：/CI/λmax-/=8.45-8=0.0/645`）：
  跨格重组超出现有 op 能力（cell 级只有整格重转写），搁置。
- **colspan 文本拆裂**（047「批准|表」横拆、「公司核心/指标」竖拆）：
  机械判定「两格拼起来是词而各自不是」可做但收益小，观察。
- ~~**dismissed 明细不进 report**~~ ✅：已补 `report.dismissedSuspects[]`，逐条展开
  `dismissed` 计数——每条带 `kind`/`itemId`/`reason`（llm_dismiss / max_rounds_exhausted /
  vision_unavailable / llm_no_tool_call / llm_error）/`detail`（LLM 一句话理由或错误信息）/
  `evidence`（探测器原始证据）。关 flag 无关恒输出，无搁置时缺省、序列化与旧版逐字节兼容。
- ~~**页眉家具被吞进 table_caption**~~ ✅（JZY full.md:549「附件3：…」+「编制人：张威」）：
  ~~同文 3 处已 drop、此处漏，待同文组联合裁决根治~~ —— 2026-06-13 重跑验证，**原诊断错误**。
  真相：泄漏成 text 的 4 处「附件3」已全 drop（it_0329/0363/0370/0376）；残留这处是
  MinerU 把页眉「附件3：…」+页脚「编制人：张威」误塞进了 page25「细分市场」跨页表片段的
  `table_caption`（输入 seq326 caption 非空），`mergeTable` 跨页合并时按「B 的 caption 字符
  不许丢」（ops.rs:208）忠实拼接保留，渲染器输出 caption → 残留。两个探测器都看不到它：
  `page_artifact` 只扫 item 文本、不扫 caption 数组；`caption_heading` 只认编号小节标题形状、
  不认「附件3/编制人」这类家具。
  **修复（2026-06-13）**：新增 `caption_artifact` 探测器——复用 page_artifact 的家具同文佐证
  （≥2 处 header/footer 同文）/全文高频重复信号扫 `table_caption` 数组，命中交 LLM 裁决
  → 新增 `dropCaption(id, captionIndex)` op（纯削减、留痕、白名单 `droppable_caption_ids` 双保险）
  或 dismiss。真 LLM 重跑：`dropCaption×2` 落地、549/551 残留清零、正文引用（131 行）保留、
  细分市场表 caption=[]、violations=0、failOpen=false、table_body 逐字节不变。

## 不做（明确排除）

- **漏字补全**（华大科技「00085」缺一位）：要加字且无法从上下文确定加什么，
  只进 report 当质量信号
- **占位符 xxxxxX 归一**：源文档本来的脱敏占位内容，改了反而违背保真
- **页眉/页码清理**：已正确分类为 `header`/`page_number`，下游过滤即可
- **「十一、」「十二、」整句小节升标题**（JZY）：原文这两节就是「编号+整句」体，
  含逗号/句末标点，表面闸正确拒绝——升成 40 字长标题反而更差
- **标题与正文粘连的「1）报告撰写责任：自 2026 年起…」**（047）：需 split+promote
  两步且切分点只能 LLM 判，同级 2）~5）已是标题、此处渲染为段落语义无损，不值得
- **源文档自身的内容错误**（JZY「波特五力」正文把五力写成「买方/卖方议价」重复、
  章节编号 3→5 跳号）：保真原则下不替作者改稿
- **同表双副本去重**（JZY p45「竞争分析表」it_0586 样式展示版 + it_0587 待填写网格版）：
  曾误判为「同表双 OCR 副本」立项，渲染 origin.pdf 后确认是**源文档作者主动画了两份同一空模板**
  （品牌标题展示版 + 大号填写网格版），非 OCR 重复检测。删一份 = 替作者改稿，同上条红线。
  两份都是空模板、单文档单次、价值有限，且任何「相邻近重复表」探测器都有误删合法相似表的
  数据丢失风险。下游若嫌冗余，自行过滤即可
