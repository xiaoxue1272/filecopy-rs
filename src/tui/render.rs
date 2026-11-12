//! 渲染层：只读 `App`，不修改状态。

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Widget, Wrap};
use ratatui::Frame;

use super::{action_color, run_status_text, BrowserEntry, Field, FormData, Mode, RunState};
use crate::model::{format_size, Strategy};

pub fn draw(f: &mut Frame, app: &super::App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(area);

    draw_title(f, chunks[0], app);
    match &app.mode {
        Mode::Form => draw_form(f, chunks[1], app),
        Mode::Browser(browser) => draw_browser(f, chunks[1], app, browser),
        Mode::Picker(picker) => draw_picker(f, chunks[1], app, picker),
        Mode::Prompt(prompt) => draw_prompt(f, chunks[1], app, prompt),
        Mode::Run(run) => draw_run(f, chunks[1], app, run),
    }
    draw_footer(f, chunks[2], app);
}

fn draw_title(f: &mut Frame, area: Rect, app: &super::App) {
    let mode_name = match app.mode {
        Mode::Form => "配置",
        Mode::Browser(_) => "选择目录",
        Mode::Picker(_) => "加载配置",
        Mode::Prompt(_) => "保存配置",
        Mode::Run(_) => "复制",
    };
    let mut spans = vec![
        Span::styled(
            " filecopy ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" v{}  ", super::version())),
        Span::styled(mode_name, Style::default().fg(Color::Cyan)),
    ];
    if let Mode::Run(run) = &app.mode {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            run_status_text(run),
            Style::default().fg(if run.summary.is_some() {
                Color::Green
            } else {
                Color::Yellow
            }),
        ));
    } else if !app.status.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            app.status.clone(),
            Style::default().fg(Color::Green),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 表单页脚：把「当前这一行能干什么」放在最前面，其余是通用按键。
fn form_footer(field: Field) -> (String, String) {
    let hint = match field {
        Field::Start | Field::Preview => "回车/空格 启动",
        Field::Source | Field::Target => "回车 选目录",
        Field::Advanced => "空格 展开/收起",
        other if other.is_choice() => "空格 改值",
        other if other.is_bool() => "空格 切换开关",
        _ => "直接输入内容",
    };
    let mut wide = String::from(hint);
    // 用「移动」而不是「切换」，免得和「空格 切换」撞词
    wide.push_str(" · ↑↓ 移动");
    // 只有并排的那几行才谈得上「换列」
    if field.pair().is_some() {
        wide.push_str(" · ←→ 换列");
    }
    wide.push_str(" · ^S 保存 · ^O 加载 · Esc 退出");
    (wide, format!("{hint} · ↑↓ 移动 · Esc 退出"))
}

fn draw_footer(f: &mut Frame, area: Rect, app: &super::App) {
    // 每种模式给长短两版，按终端宽度挑放得下的那条
    let (wide, narrow) = match &app.mode {
        Mode::Form => form_footer(app.field),
        Mode::Browser(_) => (
            "↑↓ 选择 · 回车 进入/确认 · Backspace 上一级 · h 主目录 · Esc 取消".to_string(),
            "↑↓ 选择 · 回车 确认 · Esc 取消".to_string(),
        ),
        Mode::Picker(_) => (
            "↑↓ 选择 · 回车 加载配置 · Esc 取消".to_string(),
            "回车 加载 · Esc 取消".to_string(),
        ),
        Mode::Prompt(_) => (
            "输入文件名（自动补 .json）· 回车 保存 · Esc 取消".to_string(),
            "输入文件名 · 回车 保存 · Esc 取消".to_string(),
        ),
        Mode::Run(run) => {
            if run.summary.is_some() {
                (
                    "↑↓ PgUp/PgDn 查看记录 · Esc 回表单（可改完再跑）· q 退出".to_string(),
                    "↑↓ 滚动记录 · Esc 回表单 · q 退出".to_string(),
                )
            } else {
                (
                    "Esc 中断复制 · ↑↓ PgUp/PgDn 查看记录".to_string(),
                    "Esc 中断复制 · ↑↓ 滚动".to_string(),
                )
            }
        }
    };
    let room = area.width as usize;
    let text = if display_width(&wide) < room {
        &wide
    } else {
        &narrow
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&format!(" {text}"), room),
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

const LABEL_WIDTH: usize = 14;

/// 表单里的一个分组：标题 + 若干行。
struct FormSection {
    title: &'static str,
    lines: Vec<Line<'static>>,
}

/// 表单的完整布局，行号用于滚动时把当前字段带进视野。
struct FormLayout {
    sections: Vec<FormSection>,
    /// (字段, 所属分组, 分组内行号)
    field_rows: Vec<(Field, usize, usize)>,
}

impl FormLayout {
    fn new(app: &super::App, width: u16) -> Self {
        let form = &app.form;
        let cursor = app.cursor;
        let mut layout = FormLayout {
            sections: Vec::new(),
            field_rows: Vec::new(),
        };

        layout.section(" 目录与过滤 ");
        for field in [
            Field::Source,
            Field::Target,
            Field::Extensions,
            Field::Exclude,
        ] {
            layout.field(field, field_line(app, field, form, cursor));
        }

        // 分类策略是所有设置里最要紧的一个（文件落到哪儿），必须始终可见，
        // 不能像别的开关那样收进「更多选项」里
        layout.section(" 分类策略 ");
        layout.field(
            Field::Strategy,
            field_line(app, Field::Strategy, form, cursor),
        );
        for field in [Field::DateFormat, Field::DateSource] {
            if form.field_visible(field) {
                layout.field(field, field_line(app, field, form, cursor));
            }
        }

        layout.section(" 性能 ");
        // 线程数和缓冲区都是输入框，所以各占一行：并排的话 ←→ 既想移光标又想换列
        layout.field(
            Field::Workers,
            field_line(app, Field::Workers, form, cursor),
        );
        layout.field(Field::BufferSize, {
            // 单位紧跟在数字后面，同一行；空格换单位，再往后是换算出的字节数
            let mut line = field_line(app, Field::BufferSize, form, cursor);
            let focused = app.field == Field::BufferSize;
            let unit_style = if focused {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::LightCyan)
            };
            line.spans.push(Span::styled(
                format!(" [ {} ]", super::BUFFER_UNITS[form.buffer_unit_idx].0),
                unit_style,
            ));
            let (hint, ok) = match form.buffer_bytes() {
                Some(0) => ("0 = 按默认大小（1 MB）".to_string(), true),
                Some(bytes) => (
                    format!(
                        "= {} 字节{}",
                        bytes,
                        if (4096..=64 * 1024 * 1024).contains(&bytes) {
                            ""
                        } else {
                            "（需在 4 KB – 64 MB 之间）"
                        }
                    ),
                    (4096..=64 * 1024 * 1024).contains(&bytes),
                ),
                None => ("请填不小于 0 的整数".to_string(), false),
            };
            line.spans.push(Span::styled(
                format!("  {hint}"),
                Style::default().fg(if ok { Color::DarkGray } else { Color::Red }),
            ));
            line
        });

        layout.section(" 更多选项 ");
        let mut advanced = field_line(app, Field::Advanced, form, cursor);
        advanced.spans.push(Span::styled(
            if form.advanced_open {
                "   空格 收起".to_string()
            } else {
                // 收起状态下把仍然生效的开关摘要出来，免得「藏起来的选项偷偷生效」
                let summary = advanced_summary(form);
                if summary.is_empty() {
                    "   空格 展开".to_string()
                } else {
                    format!("   空格 展开 · {summary}")
                }
            },
            Style::default().fg(Color::DarkGray),
        ));
        layout.field(Field::Advanced, advanced);

        // 展开的内容紧跟在「高级选项」这一行下面，和它同属「更多选项」这一组
        if form.advanced_open {
            let column = (width.saturating_sub(4) as usize / 2).clamp(24, 34);
            // 按屏幕顺序排；六个开关正好排成三行两列，右列不留空（见 FIELD_PAIRS）
            let mut placed = Vec::new();
            for field in super::Field::ALL {
                if !field.is_advanced_group() || placed.contains(&field) {
                    continue;
                }
                match field.pair() {
                    Some((left, right)) => {
                        let left_line = field_line(app, left, form, cursor);
                        let right_visible = form.field_visible(right);
                        let line = if right_visible {
                            pair_line(left_line, field_line(app, right, form, cursor), column)
                        } else {
                            left_line
                        };
                        layout.pair(left, if right_visible { Some(right) } else { None }, line);
                        placed.push(left);
                        placed.push(right);
                    }
                    None => {
                        layout.field(field, field_line(app, field, form, cursor));
                        placed.push(field);
                    }
                }
            }
        }

        // 两个启动按钮：并排一行，选中后回车或空格触发
        layout.section(" 操作 ");
        layout.pair(
            Field::Start,
            Some(Field::Preview),
            pair_line(
                action_line(app, Field::Start),
                action_line(app, Field::Preview),
                18,
            ),
        );

        layout
    }

    fn section(&mut self, title: &'static str) {
        self.sections.push(FormSection {
            title,
            lines: Vec::new(),
        });
    }

    fn field(&mut self, field: Field, line: Line<'static>) {
        let index = self.sections.len() - 1;
        self.field_rows
            .push((field, index, self.sections[index].lines.len()));
        self.sections[index].lines.push(line);
    }

    fn pair(&mut self, left: Field, right: Option<Field>, line: Line<'static>) {
        self.field(left, line);
        if let Some(right) = right {
            let index = self.sections.len() - 1;
            let row = self.sections[index].lines.len() - 1;
            self.field_rows.push((right, index, row));
        }
    }

    /// 所有分组（含边框）需要的总高度。
    fn height(&self) -> u16 {
        self.sections
            .iter()
            .map(|section| section.lines.len() as u16 + 2)
            .sum()
    }

    /// 某个字段在整张表单中的绝对行号。
    fn field_offset(&self, field: Field) -> u16 {
        let mut y = 0u16;
        for (index, section) in self.sections.iter().enumerate() {
            if let Some((_, _, row)) = self
                .field_rows
                .iter()
                .find(|(f, s, _)| *f == field && *s == index)
            {
                return y + 1 + *row as u16;
            }
            y += section.lines.len() as u16 + 2;
        }
        0
    }
}

fn draw_form(f: &mut Frame, area: Rect, app: &super::App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let form = &app.form;
    let layout = FormLayout::new(app, area.width);

    let hint_lines: Vec<Line<'static>> = if app.errors.is_empty() {
        let mut lines = vec![Line::from({
            // 落点是真算的：拿源目录里第一个会复制的文件，走一遍引擎那套策略计算
            let preview = app.preview();
            let mut spans = vec![Span::styled(
                " 示例落点: ",
                Style::default().fg(Color::DarkGray),
            )];
            if preview.relative.is_empty() {
                // 还没选源目录：算出来的落点无从谈起，别硬编一条给人看
                spans.push(Span::styled(
                    "（选好源目录后显示）",
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                spans.push(Span::styled(
                    preview.relative,
                    Style::default().fg(Color::LightBlue),
                ));
                if !preview.from_real_file {
                    // 说实话：这不是真实文件的落点，别让人以为扫到了文件
                    spans.push(Span::styled(
                        "（占位示例，源目录里暂无匹配文件）",
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            spans
        })];
        // 光标停在日期格式上时，给一段不吃编程知识的格式说明
        if app.field == Field::DateFormat && form.uses_date_format() {
            lines.push(Line::from(vec![
                Span::styled(" 格式说明: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    date_format_help(&form.date_format),
                    Style::default().fg(Color::Gray),
                ),
            ]));
        }
        lines
    } else {
        app.errors
            .iter()
            .take(6)
            .map(|err| {
                Line::from(Span::styled(
                    format!(" ✗ {err}"),
                    Style::default().fg(Color::Red),
                ))
            })
            .collect()
    };

    let needed = layout.height();
    // 提示行按「越靠后越重要」排列（示例落点 → 格式说明），
    // 空间不够时从前面砍，这样正在编辑日期格式时，格式说明不会先被挤掉。
    let hint_want = hint_lines.len() as u16;
    // 空间够就照常显示提示；不够时先牺牲提示，但错误信息尽量保留
    let hint_h = if needed + hint_want <= area.height {
        hint_want
    } else if !app.errors.is_empty() {
        hint_want.min(area.height.saturating_sub(1))
    } else {
        // 表单本来就会滚动，宁可少露两行表单，也别把提示整块吞掉
        let min_form = 6.min(area.height);
        hint_want.min(area.height.saturating_sub(min_form))
    };
    // 位置不够时砍掉最前面的提示行（示例落点最先走），保住更贴近当前操作的那几行
    let hint_skip = (hint_want - hint_h) as usize;
    // 每条提示都压成单行：否则长句折行会把下面的页脚顶掉
    let hint_lines: Vec<Line<'static>> = hint_lines
        .into_iter()
        .skip(hint_skip)
        .map(|line| clip_line(line, area.width))
        .collect();
    // 表单放得下时高度就等于实际需要，提示紧贴表单下方，不留大片空白
    let view_h = needed
        .min(area.height.saturating_sub(hint_h))
        .max(1)
        .min(area.height);

    let offset = if needed <= view_h {
        0
    } else {
        let selected = layout.field_offset(app.field);
        selected
            .saturating_sub(view_h / 2)
            .min(needed.saturating_sub(view_h))
    };

    // 先把整张表单画到离屏缓冲，再按滚动偏移拷进可见区域
    let mut off = Buffer::empty(Rect::new(0, 0, area.width, needed));
    let mut y = 0u16;
    for section in &layout.sections {
        let height = section.lines.len() as u16 + 2;
        let rect = Rect::new(0, y, area.width, height);
        // 超宽的行走 clip_line：不加这一刀，长路径/长说明会被边框硬切掉半个字
        let inner = area.width.saturating_sub(2);
        let lines: Vec<Line<'static>> = section
            .lines
            .iter()
            .cloned()
            .map(|line| clip_line(line, inner))
            .collect();
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(section.title)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .render(rect, &mut off);
        y += height;
    }

    {
        let frame_buffer = f.buffer_mut();
        for row in 0..view_h {
            let source_y = offset + row;
            if source_y >= needed {
                break;
            }
            for x in 0..area.width {
                if let Some(dst) = frame_buffer.cell_mut((area.x + x, area.y + row)) {
                    *dst = off[(x, source_y)].clone();
                }
            }
        }
    }

    if hint_h > 0 {
        let hint_area = Rect::new(area.x, area.y + view_h, area.width, hint_h);
        f.render_widget(Paragraph::new(hint_lines), hint_area);
    }
}

/// 一行字段：`标签  值`，当前字段高亮，文本字段带光标。
/// 启动按钮的行：`[ 开始 ]` 这种按钮样式，选中后回车/空格触发。
/// 把格式串翻译成人话：`%Y/%m/%d` → “年/月/日，例如 2026/11/08”。
fn date_format_help(fmt: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' && i + 1 < chars.len() {
            let word = match chars[i + 1] {
                'Y' => Some("年（4 位）"),
                'y' => Some("年（2 位）"),
                'm' => Some("月"),
                'd' => Some("日"),
                'H' => Some("时（24 小时）"),
                'M' => Some("分"),
                'S' => Some("秒"),
                'B' => Some("月份英文名"),
                'b' => Some("月份英文缩写"),
                'A' => Some("星期英文名"),
                'a' => Some("星期缩写"),
                'j' => Some("一年中的第几天"),
                _ => None,
            };
            if let Some(word) = word {
                parts.push(word);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    // 还没输完的格式串（比如结尾只有一个 %）就别算样例了，免得吓人
    let sample = match crate::datefmt::validate_date_format(fmt) {
        None => crate::datefmt::format_time(std::time::SystemTime::now(), fmt),
        Some(_) => String::new(),
    };
    if sample.is_empty() {
        return "格式还没写完：%Y 年 %m 月 %d 日 %H 时 %M 分 %S 秒，/ 会分出新文件夹".to_string();
    }
    if parts.is_empty() {
        return format!("照字面写，例如 {sample}；每个 / 是一层新文件夹");
    }
    format!(
        "{} → 现在会得到 {sample}；每个 / 是一层新文件夹",
        parts.join(" · ")
    )
}

/// 按显示宽度把一行裁成单行：超出部分换成 `…`，保证一行只占一行。
fn clip_line(line: Line<'static>, width: u16) -> Line<'static> {
    let limit = width as usize;
    if line.width() <= limit {
        return line;
    }
    let budget = limit.saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        let span_width = Span::raw(span.content.to_string()).width();
        if used + span_width <= budget {
            used += span_width;
            spans.push(span);
            continue;
        }
        // 这个 span 放不全：按字符累加到放得下的位置
        let mut kept = String::new();
        for ch in span.content.chars() {
            let w = Span::raw(ch.to_string()).width();
            if used + w > budget {
                break;
            }
            used += w;
            kept.push(ch);
        }
        if !kept.is_empty() {
            spans.push(Span::styled(kept, span.style));
        }
        break;
    }
    spans.push(Span::raw("…"));
    Line::from(spans)
}

fn action_line(app: &super::App, field: Field) -> Line<'static> {
    let selected = app.field == field;
    let accent = if field == Field::Start {
        Color::LightGreen
    } else {
        Color::LightCyan
    };
    // 注意：这里不能用背景色铺底 —— ratatui 不会给双宽字符的续格上样式，
    // 中文按钮铺底色后右边半格会漏底、字被切一半。选中状态改用加粗 + 提亮。
    let button_style = if selected {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[ {} ]", field.label()), button_style),
    ])
}

fn field_line(app: &super::App, field: Field, form: &FormData, cursor: usize) -> Line<'static> {
    if field.is_action() {
        return action_line(app, field);
    }
    let selected = app.field == field;
    let mut spans = vec![label_span(field, selected)];

    if field == Field::Advanced {
        // 展开状态本身就是这个字段的「值」
        let open = form.advanced_open;
        let style = Style::default()
            .fg(if open { Color::Yellow } else { Color::DarkGray })
            .add_modifier(if open {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        spans.push(Span::styled(
            if open { " [已展开]" } else { " [已收起]" },
            style,
        ));
    } else if field.is_bool() {
        let on = form.bool_of(field).unwrap_or(false);
        let style = Style::default()
            .fg(if on { Color::Green } else { Color::DarkGray })
            .add_modifier(if on {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        spans.push(Span::styled(if on { " [开] " } else { " [关] " }, style));
    } else if field.is_choice() {
        let value = form.text(field);
        if selected {
            spans.push(Span::styled(
                // 不用 ◀ ▶：很多终端字体缺这两个字形，且左右键现在是换列
                format!(" [{value}] "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!(" [ {value} ]"),
                Style::default().fg(Color::White),
            ));
        }
    } else {
        // 文本字段：当前字段把光标位置画成块
        let value = form.text(field);
        if selected {
            let chars: Vec<char> = value.chars().collect();
            let pos = cursor.min(chars.len());
            let before: String = chars[..pos].iter().collect();
            let at: String = chars
                .get(pos)
                .map(|c| c.to_string())
                .unwrap_or_else(|| " ".into());
            let after: String = chars
                .get(pos + 1..)
                .map(|s| s.iter().collect())
                .unwrap_or_default();
            spans.push(Span::styled(
                format!(" {before}"),
                Style::default().fg(Color::White),
            ));
            spans.push(Span::styled(
                at,
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ));
            spans.push(Span::styled(after, Style::default().fg(Color::White)));
        } else if value.is_empty() {
            spans.push(Span::styled(
                " (未填写)",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            // 选中态会先画一个空格再接光标块，这里补同样的空格，整列才对得齐
            spans.push(Span::styled(
                format!(" {value}"),
                Style::default().fg(Color::White),
            ));
        }
    }
    // 策略后面紧跟一句人话说明：它和「分类策略」同一行，不用去底部找
    if field == Field::Strategy {
        let strategy = Strategy::ALL[form.strategy_idx.min(Strategy::ALL.len() - 1)];
        spans.push(Span::styled(
            format!(
                " {}",
                strategy.describe(&form.date_format, form.date_source())
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // 日期格式后面直接给个「算出来长什么样」，不用懂 %Y 也能看懂
    if field == Field::DateFormat && form.uses_date_format() {
        let fmt = form.date_format.trim();
        if !fmt.is_empty() && crate::datefmt::validate_date_format(fmt).is_none() {
            let sample = crate::datefmt::format_time(std::time::SystemTime::now(), fmt);
            if !sample.is_empty() {
                spans.push(Span::styled(
                    format!("  → {sample}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
    }
    Line::from(spans)
}

/// 标签列按显示宽度补齐，双宽中文也能对齐。
fn label_span(field: Field, selected: bool) -> Span<'static> {
    let marker = if selected { ">" } else { " " };
    let text = format!("{marker}{}", field.label());
    let pad = LABEL_WIDTH.saturating_sub(Span::raw(text.clone()).width());
    let padded = format!("{text}{}", " ".repeat(pad));
    let style = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Span::styled(padded, style)
}

/// 同一行放两个字段：左边补齐到固定列宽，再拼右边。
fn pair_line(left: Line<'static>, right: Line<'static>, column: usize) -> Line<'static> {
    let mut line = left;
    let width = line.width();
    let pad = column.saturating_sub(width).max(1);
    line.spans.push(Span::raw(" ".repeat(pad)));
    let mut rhs = right;
    line.spans.append(&mut rhs.spans);
    line
}

/// 收起「高级选项」时，用它生成一行摘要，说明哪些高级设置在生效。
fn advanced_summary(form: &FormData) -> String {
    // 分类策略和日期格式已经常驻在表单里，这里只摘要真正被收起的那几项
    // 顺序跟表单里的排布一致（第一行 → 最后一行），扫一眼就能对上那一行
    let mut parts = Vec::new();
    let policy = form.conflict_idx.min(super::CONFLICT_POLICIES.len() - 1);
    if policy > 0 {
        parts.push(format!("同名:{}", super::CONFLICT_POLICIES[policy]));
    }
    if form.fail_empty {
        parts.push("空扫描失败".to_string());
    }
    if form.verify {
        let algo =
            super::VERIFY_ALGORITHMS[form.verify_algo_idx.min(super::VERIFY_ALGORITHMS.len() - 1)];
        parts.push(format!("校验 {algo}"));
    }
    if form.include_hidden {
        parts.push("含隐藏".to_string());
    }
    if !form.recursive {
        parts.push("不递归".to_string());
    }
    parts.join(" · ")
}

/// 居中的对话框区域：不超过给定尺寸，也不超出可用区域。
fn centered(area: Rect, want_w: u16, want_h: u16) -> Rect {
    let width = want_w.min(area.width);
    let height = want_h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// 列表行：选中时用 `>` 标记 + 青色加粗，避免给中文铺底色时宽字符被擦掉。
fn selectable_line(text: &str, selected: bool, base: Style) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let style = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        base
    };
    Line::from(Span::styled(format!("{marker}{text}"), style))
}

fn draw_browser(f: &mut Frame, area: Rect, app: &super::App, browser: &super::BrowserState) {
    let count = browser.entries.len();
    let rect = centered(area, 66, (count as u16 + 5).clamp(9, 22));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" 选择目录 · {} ", browser.field.label()))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height < 3 {
        return;
    }

    let width = inner.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            truncate(&format!(" {}", browser.dir.to_string_lossy()), width),
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let list_height = inner.height.saturating_sub(2) as usize;
    let offset = visible_offset(browser.selected, count, list_height, app.browser_scroll);
    for (index, entry) in browser
        .entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(list_height)
    {
        let base = match entry {
            BrowserEntry::Parent | BrowserEntry::ChooseHere => {
                Style::default().fg(Color::LightBlue)
            }
            BrowserEntry::Dir(_) => Style::default().fg(Color::White),
        };
        lines.push(selectable_line(
            &entry.label(),
            index == browser.selected,
            base,
        ));
    }
    if let Some(err) = &browser.error {
        lines.push(Line::from(Span::styled(
            format!(" ✗ {err}"),
            Style::default().fg(Color::Red),
        )));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_picker(f: &mut Frame, area: Rect, app: &super::App, picker: &super::PickerState) {
    let count = picker.files.len();
    let rect = centered(area, 60, (count as u16 + 4).clamp(7, 20));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" 加载配置 · 找到 {count} 个 "))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let list_height = inner.height as usize;
    let offset = visible_offset(picker.selected, count, list_height, app.picker_scroll);
    let mut lines: Vec<Line> = Vec::new();
    for (index, path) in picker
        .files
        .iter()
        .enumerate()
        .skip(offset)
        .take(list_height)
    {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        lines.push(selectable_line(
            &truncate(&name, inner.width as usize - 2),
            index == picker.selected,
            Style::default().fg(Color::White),
        ));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_prompt(f: &mut Frame, area: Rect, _app: &super::App, prompt: &super::PromptState) {
    let rect = centered(area, 56, 4);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 保存配置 ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let chars: Vec<char> = prompt.input.chars().collect();
    let pos = prompt.cursor.min(chars.len());
    let before: String = chars[..pos].iter().collect();
    let at: String = chars
        .get(pos)
        .map(|c| c.to_string())
        .unwrap_or_else(|| " ".into());
    let after: String = chars
        .get(pos + 1..)
        .map(|s| s.iter().collect())
        .unwrap_or_default();

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" 文件名 ", Style::default().fg(Color::DarkGray)),
            Span::raw(before),
            Span::styled(at, Style::default().fg(Color::Black).bg(Color::Cyan)),
            Span::raw(after),
        ]),
        Line::from(Span::styled(
            prompt
                .error
                .as_ref()
                .map(|err| format!(" ✗ {err}"))
                .unwrap_or_else(|| " 回车保存，自动补 .json".to_string()),
            Style::default().fg(if prompt.error.is_some() {
                Color::Red
            } else {
                Color::DarkGray
            }),
        )),
    ];
    if inner.height as usize > lines.len() {
        lines.push(Line::from(""));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_run(f: &mut Frame, area: Rect, app: &super::App, run: &super::RunState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(7),
        ])
        .margin(1)
        .split(area);

    let file_ratio = if run.files_total == 0 {
        0.0
    } else {
        (run.files_done as f64 / run.files_total as f64).clamp(0.0, 1.0)
    };
    f.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" 文件 "))
            .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
            .ratio(file_ratio)
            .label(format!("{} / {}", run.files_done, run.files_total)),
        chunks[0],
    );

    let byte_ratio = if run.bytes_total == 0 {
        0.0
    } else {
        (run.bytes_done as f64 / run.bytes_total as f64).clamp(0.0, 1.0)
    };
    f.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" 数据量 "))
            .gauge_style(Style::default().fg(Color::LightGreen).bg(Color::Black))
            .ratio(byte_ratio)
            .label(format!(
                "{} / {}",
                format_size(run.bytes_done),
                format_size(run.bytes_total)
            )),
        chunks[1],
    );

    draw_records(f, chunks[2], app, run);
    draw_logs(f, chunks[3], run);
}

/// 表格列宽（按显示宽度算，中文标签才对齐）
const RECORD_ACTION_W: usize = 9;
const RECORD_SIZE_W: usize = 10;
const RECORD_DETAIL_W: usize = 28;

fn draw_records(f: &mut Frame, area: Rect, _app: &super::App, run: &RunState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " 复制记录 {} 条{} ",
            run.records.len(),
            if run.follow {
                ""
            } else {
                "（已暂停跟随）"
            }
        ))
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let height = inner.height as usize;
    if height == 0 {
        return;
    }
    let width = inner.width as usize;
    let name_w = width
        .saturating_sub(RECORD_ACTION_W + RECORD_SIZE_W + RECORD_DETAIL_W)
        .max(12);

    // 前两行是表头与分隔线
    let list_height = height.saturating_sub(2);
    let total = run.records.len();
    let offset = if total <= list_height {
        0
    } else {
        run.scroll
            .saturating_sub(list_height.saturating_sub(1))
            .min(total - list_height)
    };

    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(pad_display(" 动作", RECORD_ACTION_W), header_style),
            Span::styled(pad_display("大小", RECORD_SIZE_W), header_style),
            Span::styled(pad_display("文件名", name_w), header_style),
            Span::styled("说明", header_style),
        ]),
        Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    for rec in run.records.iter().skip(offset).take(list_height) {
        let name = rec
            .src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| rec.src.display().to_string());
        let name = truncate(&name, name_w);
        // 源文件可能已被移走，取不到大小时显示 "-" 而不是骗人的 0 bytes
        let size = match rec.src.metadata() {
            Ok(meta) => format_size(meta.len()),
            Err(_) => "-".to_string(),
        };
        lines.push(Line::from(vec![
            Span::styled(
                pad_display(&format!(" {}", rec.action.label()), RECORD_ACTION_W),
                Style::default().fg(action_color(rec.action)),
            ),
            Span::styled(
                pad_display(&size, RECORD_SIZE_W),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                pad_display(&name, name_w),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                truncate(&rec.detail, RECORD_DETAIL_W),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    if total == 0 {
        lines.push(Line::from(Span::styled(
            " 等待扫描…",
            Style::default().fg(Color::DarkGray),
        )));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_logs(f: &mut Frame, area: Rect, run: &RunState) {
    let lines: Vec<Line> = run
        .logs
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(|l| {
            Line::from(Span::styled(
                format!(" {l}"),
                Style::default().fg(Color::Gray),
            ))
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 日志 ")
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

/// 需要时滚动到可见窗口。
fn visible_offset(selected: usize, total: usize, height: usize, _scroll: usize) -> usize {
    if total <= height || height == 0 {
        return 0;
    }
    if selected < height {
        0
    } else {
        (selected + 1 - height).min(total.saturating_sub(height))
    }
}

/// 字符串的终端显示宽度（中文算两格）。
fn display_width(s: &str) -> usize {
    Span::raw(s.to_string()).width()
}

/// 按显示宽度补空格。
fn pad_display(s: &str, width: usize) -> String {
    let used = display_width(s);
    if used >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - used))
    }
}

/// 按显示宽度截断，超长补省略号。
fn truncate(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = display_width(&ch.to_string());
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::engine::Summary;
    use crate::model::{CopyRecord, FileAction};
    use crate::tui::{App, BrowserEntry, BrowserState, Mode, PickerState, RunState};
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::channel;
    use std::sync::Arc;

    /// 把绘制结果导出成 JSON（字符 + 颜色），供 tools/screenshot.py 生成预览图。
    ///
    /// 默认什么都不做：只有设置 FILECOPY_DUMP 目录才会写文件，避免跑测试时往磁盘丢垃圾。
    #[test]
    fn dump_screens_for_docs() {
        let Ok(dir) = std::env::var("FILECOPY_DUMP") else {
            return;
        };
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (width, height) = (104u16, 30u16);

        // 截图里的「示例落点」现在走真实计算，所以得造一棵真实存在的小目录：
        // 固定用 /tmp 下的短路径，比 /var/folders/xxxx/T 好读，也不占空间。
        let sink = PathBuf::from("/tmp/filecopy-dump");
        let source = sink.join("待归档").join("第一批");
        std::fs::create_dir_all(&source).unwrap();
        for (name, size) in [
            ("文件-001.jpg", 3_400_000u64),
            ("文件-002.png", 4_100_000),
            ("文件-003.jpg", 2_000_000),
            ("视频-001.mp4", 1_200_000),
            ("视频-002.mp4", 3_300_000),
        ] {
            let file = std::fs::File::create(source.join(name)).unwrap();
            file.set_len(size).unwrap();
        }

        // 1) 空表单
        let empty = App::new(None);
        std::fs::write(dir.join("form-empty.json"), dump(&empty, width, height)).unwrap();

        // 2) 填好的表单
        let mut config = crate::config::Config::default();
        config.source_dir = sink.join("待归档").display().to_string();
        config.target_dir = "/tmp/filecopy-dump-out".into();
        config.extensions = vec!["jpg".into(), "nef".into(), "mp4".into()];
        config.exclude = vec!["*.tmp".into()];
        config.strategy = "date_tree".into();
        config.workers = 4;
        config.verify = true;
        config.auto_rename = true;
        let mut filled = App::new(Some(config.clone()));
        // 光标放到文本末尾，截图里就不会在路径中间冒出一块高亮
        filled.cursor = filled.form.text(filled.field).chars().count();

        // 收起状态（默认）与展开状态各来一张
        let mut expanded = App::new(Some(config.clone()));
        // 模拟「在高级选项上按空格」后的状态：展开且光标已在第一行
        expanded.field = Field::Advanced;
        expanded.toggle_advanced();
        expanded.cursor = expanded.form.text(expanded.field).chars().count();
        std::fs::write(dir.join("form-filled.json"), dump(&filled, width, height)).unwrap();
        std::fs::write(
            dir.join("form-expanded.json"),
            dump(&expanded, width, height),
        )
        .unwrap();

        // 2d) 光标停在「日期格式」上：说明行与日期来源一起出现
        let mut datefmt = App::new(Some(config.clone()));
        datefmt.field = Field::Advanced;
        datefmt.toggle_advanced();
        datefmt.focus(Field::DateFormat);
        std::fs::write(
            dir.join("form-date-format.json"),
            dump(&datefmt, width, height),
        )
        .unwrap();
        std::fs::write(
            dir.join("form-date-format-small.json"),
            dump(&datefmt, 76, 22),
        )
        .unwrap();

        // 2c) 选中启动按钮的样子
        let mut buttons = App::new(None);
        buttons.field = Field::Start;
        std::fs::write(dir.join("form-buttons.json"), dump(&buttons, width, height)).unwrap();

        // 2b) 小终端：表单放不下时应该滚动到当前字段
        let mut small_app = App::new(Some(config));
        small_app.form.advanced_open = true;
        small_app.field = Field::FailEmpty;
        std::fs::write(dir.join("form-small.json"), dump(&small_app, 76, 20)).unwrap();

        // 3) 目录浏览器 / 配置选择 / 保存对话框
        let browser = BrowserState {
            field: Field::Source,
            dir: PathBuf::from("/Users/me/Downloads"),
            entries: vec![
                BrowserEntry::Parent,
                BrowserEntry::ChooseHere,
                BrowserEntry::Dir(PathBuf::from("/Users/me/Downloads/待归档")),
                BrowserEntry::Dir(PathBuf::from("/Users/me/Downloads/文档")),
                BrowserEntry::Dir(PathBuf::from("/Users/me/Downloads/备份")),
            ],
            selected: 2,
            error: None,
        };
        let mut browser_app = App::new(None);
        browser_app.mode = Mode::Browser(browser);
        std::fs::write(dir.join("browser.json"), dump(&browser_app, width, height)).unwrap();

        let picker = PickerState {
            files: vec![
                PathBuf::from("archive-2026.json"),
                PathBuf::from("downloads-daily.json"),
                PathBuf::from("old-legacy.json"),
            ],
            selected: 0,
        };
        let mut picker_app = App::new(None);
        picker_app.mode = Mode::Picker(picker);
        std::fs::write(dir.join("picker.json"), dump(&picker_app, width, height)).unwrap();

        let mut prompt_app = App::new(None);
        prompt_app.mode = Mode::Prompt(crate::tui::PromptState {
            input: "archive-2026".into(),
            cursor: 12,
            error: None,
        });
        std::fs::write(dir.join("prompt.json"), dump(&prompt_app, width, height)).unwrap();

        // 3) 跑完的运行界面（含复制/覆盖/跳过/失败四类记录）
        let mut summary = Summary::default();
        summary.scanned_files = 5;
        summary.scanned_bytes = 15_000_000;
        summary.copied_files = 3;
        summary.copied_bytes = 9_500_000;
        summary.skipped_files = 1;
        summary.overrode_files = 1;
        summary.failed_files = 1;
        summary.verified_files = 3;
        summary.planned_files = 5;
        summary.pending_bytes = 9_500_000;

        // 记录里的大小是渲染时读源文件得到的，文件在上面已经造好了
        let record = |name: &str, action: FileAction, detail: &str, dst_name: &str| CopyRecord {
            src: source.join(name),
            dst: PathBuf::from(format!(
                "/tmp/filecopy-dump-out/2026/11/11/待归档/第一批/{dst_name}"
            )),
            action,
            detail: detail.to_string(),
        };
        let records = vec![
            record(
                "文件-001.jpg",
                FileAction::Copied,
                "sha256 校验通过",
                "文件-001.jpg",
            ),
            record(
                "文件-002.png",
                FileAction::Copied,
                "sha256 校验通过",
                "文件-002.png",
            ),
            record(
                "文件-003.jpg",
                FileAction::Overwritten,
                "sha256 校验通过",
                "文件-003_1.jpg",
            ),
            record(
                "视频-001.mp4",
                FileAction::Skipped,
                "目标已存在，未覆盖",
                "视频-001.mp4",
            ),
            record(
                "视频-002.mp4",
                FileAction::Failed,
                "No space left on device (os error 28)",
                "视频-002.mp4",
            ),
        ];

        let (_tx, rx) = channel();
        let run = RunState {
            rx,
            handle: None,
            cancel: Arc::new(AtomicBool::new(false)),
            records,
            logs: vec![
                "扫描结果: 找到 5 个文件，总大小 14.3 MB".to_string(),
                "分类策略: 按创建日期（%Y/%m/%d）+ 保留源目录结构".to_string(),
                "并行复制: 4 个线程".to_string(),
                "校验和验收: sha256（复制后比对）".to_string(),
                "成功 3 个（覆盖 1），跳过 1，失败 1".to_string(),
            ],
            files_done: 5,
            files_total: 5,
            bytes_done: 9_500_000,
            bytes_total: 9_500_000,
            summary: Some(summary),
            scroll: 0,
            follow: true,
        };
        let mut app = App::new(None);
        app.mode = Mode::Run(run);
        std::fs::write(dir.join("run-finished.json"), dump(&app, width, height)).unwrap();

        // 4) 很矮的终端：表单放不下，应该滚动到当前字段
        let mut tiny = App::new(None);
        tiny.form.advanced_open = true;
        tiny.form.verify = true;
        tiny.field = Field::VerifyAlgo;
        std::fs::write(dir.join("form-tiny.json"), dump(&tiny, 62, 14)).unwrap();
    }

    fn dump(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();

        let mut cells = Vec::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                let symbol = cell.symbol().to_string();
                if symbol.is_empty() {
                    continue; // 双宽字符的占位单元
                }
                let reversed = cell.modifier.contains(Modifier::REVERSED);
                let (fg, bg) = if reversed {
                    (color_hex(cell.bg, "#1c1c1c"), color_hex(cell.fg, "#e6e6e6"))
                } else {
                    (color_hex(cell.fg, "#e6e6e6"), color_hex(cell.bg, "#1c1c1c"))
                };
                cells.push(format!(
                    r#"{{"x":{x},"y":{y},"ch":"{}","fg":"{fg}","bg":"{bg}","bold":{}}}"#,
                    symbol.replace('\\', "\\\\").replace('"', "\\\""),
                    cell.modifier.contains(Modifier::BOLD),
                ));
            }
        }
        format!(
            r#"{{"width":{},"height":{},"cells":[{}]}}"#,
            buffer.area.width,
            buffer.area.height,
            cells.join(",")
        )
    }

    fn color_hex(color: Color, fallback: &str) -> String {
        match color {
            Color::Reset => fallback.to_string(),
            Color::Black => "#1c1c1c".into(),
            Color::Red => "#e06c75".into(),
            Color::Green => "#98c379".into(),
            Color::Yellow => "#e5c07b".into(),
            Color::Blue => "#61afef".into(),
            Color::Magenta => "#c678dd".into(),
            Color::Cyan => "#56b6c2".into(),
            Color::Gray => "#abb2bf".into(),
            Color::DarkGray => "#5c6370".into(),
            Color::LightRed => "#ff7b86".into(),
            Color::LightGreen => "#b6e29a".into(),
            Color::LightYellow => "#ffd68a".into(),
            Color::LightBlue => "#83c4ff".into(),
            Color::LightMagenta => "#dda0f5".into(),
            Color::LightCyan => "#7fd6e0".into(),
            Color::White => "#e6e6e6".into(),
            Color::Indexed(i) => match i {
                0 => "#1c1c1c".into(),
                8 => "#5c6370".into(),
                15 => "#e6e6e6".into(),
                _ => "#abb2bf".into(),
            },
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }
}
