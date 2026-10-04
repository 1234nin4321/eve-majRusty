use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BorderStyle {
    Solid,
    Dashed,
    Dotted,
    Double,
    DiagonalHatch,
    DashDot,
    CornerBrackets,
}

/// Visual style for the thumbnail overlay shown on characters excluded from hotkey cycling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExclusionOverlayStyle {
    X,
    DiagonalSlash,
    DiagonalHatch,
    Checkerboard,
    SolidTint,
    CircleSlash,
    None,
}

/// Animation style for window operations (restore, minimize)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AnimationStyle {
    OriginalAnimation,
    NoAnimation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClickTrigger {
    MouseDown,
    MouseUp,
}

/// System cursor shown while hovering a thumbnail; `Default` leaves the window class's arrow in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HoverCursor {
    Default,
    Hand,
    Crosshair,
    Move,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextPosition {
    TopLeft,
    TopCenter,
    TopRight,
    LeftCenter,
    Center,
    RightCenter,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FontWeight {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl FontWeight {
    /// Convert to Windows font weight value (for CreateFontA weight parameter)
    pub fn to_win32_weight(self) -> i32 {
        // 400/700 are the raw FW_NORMAL/FW_BOLD values
        match self {
            Self::Regular | Self::Italic => 400,
            Self::Bold | Self::BoldItalic => 700,
        }
    }

    /// Check if font should be italicized (for CreateFontA italic parameter)
    pub fn is_italic(self) -> bool {
        matches!(self, Self::Italic | Self::BoldItalic)
    }
}

macro_rules! notification_types {
    ($($name:ident),* $(,)?) => {
        /// Notification event types from EVE game logs
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum NotificationType {
            $($name,)*
        }

        impl NotificationType {
            /// Every variant, in declaration order (the order profiles list them in).
            pub const ALL: &'static [NotificationType] = &[$(NotificationType::$name,)*];
            pub const COUNT: usize = Self::ALL.len();

            pub fn as_str(self) -> &'static str {
                match self {
                    $(NotificationType::$name => stringify!($name),)*
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $(stringify!($name) => Some(NotificationType::$name),)*
                    _ => None,
                }
            }

            /// Position in ALL, for per-type tables.
            pub fn index(self) -> usize {
                self as usize
            }
        }
    };
}

notification_types! {
    FleetInvite,
    FleetFollow,
    FleetRegroup,
    FleetDisband,
    ConversationInvite,
    JumpCloning,
    MiningCompression,
    AsteroidDepleted,
    MiningIdle,
    MiningStopped,
    CargoFull,
    TakingDamage,
    WarpScrambled,
    WarpDisrupted,
    Decloak,
    ObservatoryDecloak,
    CloakFailed,
    CrystalBroke,
    BombLauncherEmpty,
    SelfDestruct,
    Docking,
    AutopilotReached,
    AutopilotApproaching,
    JumpRange,
    AggressionCantJump,
    WarpBubble,
    ConduitJump,
    SystemChange,
    TravelLeftBehind,
    Generic,
}

/// Mirrors the 5 categories config_dialog.js's NOTIFICATION_TYPES groups these into, for the History Panel's category filter buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotificationCategory {
    Fleet,
    Mining,
    Combat,
    Navigation,
    General,
}

impl NotificationType {
    pub fn category(self) -> NotificationCategory {
        use NotificationCategory as C;
        use NotificationType::*;
        match self {
            FleetInvite | FleetFollow | FleetRegroup | FleetDisband => C::Fleet,
            MiningCompression | AsteroidDepleted | MiningIdle | MiningStopped | CargoFull | CrystalBroke => C::Mining,
            TakingDamage | WarpScrambled | WarpDisrupted | Decloak | ObservatoryDecloak | CloakFailed
            | BombLauncherEmpty | SelfDestruct | WarpBubble => C::Combat,
            Docking | AutopilotReached | AutopilotApproaching | JumpRange | AggressionCantJump | ConduitJump
            | JumpCloning | SystemChange | TravelLeftBehind => C::Navigation,
            ConversationInvite | Generic => C::General,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayoutMode {
    Custom,
    RegionFit,
}

/// Primary display mode: how EVE clients are presented
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ViewMode {
    Thumbnails,
    ClientList,
    Nothing,
}

/// Ordering mode for rows in the compact client list view
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ListViewOrder {
    Tracked,
    Alphabetical,
    ConfiguredCharacters,
}

/// Fill order for RegionFit's grid: configured character list, or grouped by hotkey group membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RegionFitOrder {
    Characters,
    HotkeyGroups,
}

/// Fill direction for RegionFit's grid
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RegionFitDirection {
    RowFirst_LTR_TTB,
    RowFirst_RTL_TTB,
    RowFirst_LTR_BTT,
    RowFirst_RTL_BTT,
    ColumnFirst_TTB_LTR,
    ColumnFirst_BTT_LTR,
    ColumnFirst_TTB_RTL,
    ColumnFirst_BTT_RTL,
}
