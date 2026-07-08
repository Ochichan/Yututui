use crate::ui::mascot::asset::{MascotAsset, MascotFrame, MascotStyle};

pub static DJ_GEM_IDLE: MascotAsset = MascotAsset {
    name: "dj_gem_idle",
    width: 24,
    height: 15,
    fps: 3,
    looped: true,
    fallback: Some(&DJ_GEM_IDLE_RETRO),
    frames: &[MascotFrame {
        hold: 1,
        style: MascotStyle::Accent,
        lines: &IDLE_FRAME,
    }],
};

pub static DJ_GEM_IDLE_RETRO: MascotAsset = MascotAsset {
    name: "dj_gem_idle_retro",
    width: 24,
    height: 15,
    fps: 3,
    looped: true,
    fallback: None,
    frames: &[MascotFrame {
        hold: 1,
        style: MascotStyle::Accent,
        lines: &IDLE_FRAME,
    }],
};

pub static DJ_GEM_GROOVE: MascotAsset = MascotAsset {
    name: "dj_gem_groove",
    width: 24,
    height: 15,
    fps: 3,
    looped: true,
    fallback: Some(&DJ_GEM_GROOVE_RETRO),
    frames: &[
        MascotFrame {
            hold: 1,
            style: MascotStyle::Accent,
            lines: &IDLE_FRAME,
        },
        MascotFrame {
            hold: 1,
            style: MascotStyle::Accent,
            lines: &GROOVE_FRAME_A,
        },
        MascotFrame {
            hold: 1,
            style: MascotStyle::Accent,
            lines: &GROOVE_FRAME_B,
        },
        MascotFrame {
            hold: 1,
            style: MascotStyle::Accent,
            lines: &GROOVE_FRAME_C,
        },
    ],
};

pub static DJ_GEM_GROOVE_RETRO: MascotAsset = MascotAsset {
    name: "dj_gem_groove_retro",
    width: 24,
    height: 15,
    fps: 3,
    looped: true,
    fallback: None,
    frames: &[
        MascotFrame {
            hold: 1,
            style: MascotStyle::Accent,
            lines: &IDLE_FRAME,
        },
        MascotFrame {
            hold: 1,
            style: MascotStyle::Accent,
            lines: &GROOVE_FRAME_A,
        },
        MascotFrame {
            hold: 1,
            style: MascotStyle::Accent,
            lines: &GROOVE_FRAME_B,
        },
        MascotFrame {
            hold: 1,
            style: MascotStyle::Accent,
            lines: &GROOVE_FRAME_C,
        },
    ],
};

pub static DJ_GEM_THINKING: MascotAsset = MascotAsset {
    name: "dj_gem_thinking",
    width: 24,
    height: 15,
    fps: 3,
    looped: true,
    fallback: Some(&DJ_GEM_THINKING_RETRO),
    frames: &[
        MascotFrame {
            hold: 2,
            style: MascotStyle::Thinking,
            lines: &THINKING_FRAME_A,
        },
        MascotFrame {
            hold: 2,
            style: MascotStyle::Thinking,
            lines: &THINKING_FRAME_B,
        },
    ],
};

pub static DJ_GEM_THINKING_RETRO: MascotAsset = MascotAsset {
    name: "dj_gem_thinking_retro",
    width: 24,
    height: 15,
    fps: 3,
    looped: true,
    fallback: None,
    frames: &[
        MascotFrame {
            hold: 2,
            style: MascotStyle::Thinking,
            lines: &THINKING_FRAME_A,
        },
        MascotFrame {
            hold: 2,
            style: MascotStyle::Thinking,
            lines: &THINKING_FRAME_B,
        },
    ],
};

const IDLE_FRAME: [&str; 15] = [
    "                        ",
    "      /\\        /\\      ",
    "     /  \\______/  \\     ",
    "    /              \\    ",
    "   |   o      o     |   ",
    "   |       v        |   ",
    "   |    \\____/      |   ",
    "    \\     ||      /     ",
    "     `----||-----`      ",
    "        /====\\          ",
    "     * / DJ  \\ *        ",
    "      / GEM   \\         ",
    "     /________\\         ",
    "        ||  ||          ",
    "       _||  ||_         ",
];

const GROOVE_FRAME_A: [&str; 15] = [
    "                        ",
    "      /\\    *   /\\      ",
    "     /  \\______/  \\     ",
    "    /              \\    ",
    "   |   *      o     |   ",
    "   |       v        |   ",
    "   |    \\____/      |   ",
    "    \\   \\ ||      /     ",
    "     `---\\||-----`      ",
    "        /====\\          ",
    "      */ DJ  \\          ",
    "      / GEM   \\ *       ",
    "     /________\\         ",
    "        ||  ||          ",
    "       _||  ||_         ",
];

const GROOVE_FRAME_B: [&str; 15] = [
    "                        ",
    "      /\\        /\\      ",
    "   * /  \\______/  \\     ",
    "    /              \\    ",
    "   |   o      *     |   ",
    "   |       v        |   ",
    "   |    \\____/      |   ",
    "    \\      || /    /    ",
    "     `-----||/---`      ",
    "        /====\\          ",
    "        /  DJ \\ *       ",
    "    *  /  GEM \\         ",
    "      /________\\        ",
    "       ||    ||         ",
    "      _||    ||_        ",
];

const GROOVE_FRAME_C: [&str; 15] = [
    "                        ",
    "      /\\        /\\      ",
    "     /  \\______/* \\     ",
    "    /              \\    ",
    "   |   *      *     |   ",
    "   |       v        |   ",
    "   |    \\____/      |   ",
    "    \\     ||   /  /     ",
    "     `----||--/--`      ",
    "        /====\\          ",
    "      */ DJ  \\ *        ",
    "      / GEM   \\         ",
    "     /________\\         ",
    "       ||    ||         ",
    "      _||    ||_        ",
];

const THINKING_FRAME_A: [&str; 15] = [
    "                        ",
    "      /\\   ?    /\\      ",
    "     /  \\______/  \\     ",
    "    /              \\    ",
    "   |   o      o     |   ",
    "   |       -        |   ",
    "   |      ___       |   ",
    "    \\     ||      /     ",
    "     `----||-----`      ",
    "        /====\\          ",
    "       / DJ  \\ .        ",
    "      / GEM   \\ ..      ",
    "     /________\\ ...     ",
    "        ||  ||          ",
    "       _||  ||_         ",
];

const THINKING_FRAME_B: [&str; 15] = [
    "                        ",
    "      /\\    ?   /\\      ",
    "     /  \\______/  \\     ",
    "    /              \\    ",
    "   |   o      o     |   ",
    "   |       -        |   ",
    "   |      ___       |   ",
    "    \\     ||      /     ",
    "     `----||-----`      ",
    "        /====\\          ",
    "       / DJ  \\ ...      ",
    "      / GEM   \\ .       ",
    "     /________\\ ..      ",
    "        ||  ||          ",
    "       _||  ||_         ",
];
