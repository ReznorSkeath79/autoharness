//! Pure retained layout intent for the AutoHarness cockpit.

pub const DEFAULT_SIDEBAR_WIDTH: f32 = crate::theme::SIDEBAR_WIDTH;
pub const DEFAULT_INSPECTOR_WIDTH: f32 = crate::theme::Metrics::INSPECTOR_WIDTH;
pub const DEFAULT_COORDINATOR_FRACTION: f32 = 0.55;
const MIN_SIDEBAR_WIDTH: f32 = 220.0;
const MAX_SIDEBAR_WIDTH: f32 = 400.0;
const MIN_INSPECTOR_WIDTH: f32 = 300.0;
const MAX_INSPECTOR_WIDTH: f32 = 560.0;
const MIN_COORDINATOR_HEIGHT: f32 = 260.0;
const MIN_EXECUTION_HEIGHT: f32 = 150.0;
pub const MIN_CENTER_WIDTH: f32 = 360.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnWidths {
    pub sidebar: f32,
    pub inspector: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowHeights {
    pub coordinator: f32,
    pub execution: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShellLayout {
    pub sidebar_open: bool,
    pub inspector_open: bool,
    pub execution_open: bool,
    sidebar_width: f32,
    inspector_width: f32,
    coordinator_fraction: f32,
}

impl Default for ShellLayout {
    fn default() -> Self {
        Self {
            sidebar_open: true,
            inspector_open: true,
            execution_open: true,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            inspector_width: DEFAULT_INSPECTOR_WIDTH,
            coordinator_fraction: DEFAULT_COORDINATOR_FRACTION,
        }
    }
}

impl ShellLayout {
    pub fn columns(self) -> ColumnWidths {
        ColumnWidths {
            sidebar: if self.sidebar_open {
                self.sidebar_width
            } else {
                0.0
            },
            inspector: if self.inspector_open {
                self.inspector_width
            } else {
                0.0
            },
        }
    }

    pub fn columns_for_body(self, available_width: f32) -> ColumnWidths {
        let mut columns = self.columns();
        if columns.sidebar + columns.inspector <= body_panel_budget(available_width) {
            return columns;
        }

        if self.inspector_open {
            columns.inspector = clamp_panel_width(
                columns.inspector,
                MIN_INSPECTOR_WIDTH,
                body_panel_max(available_width, columns.sidebar).min(MAX_INSPECTOR_WIDTH),
            );
        }
        if self.sidebar_open {
            columns.sidebar = clamp_panel_width(
                columns.sidebar,
                MIN_SIDEBAR_WIDTH,
                body_panel_max(available_width, columns.inspector).min(MAX_SIDEBAR_WIDTH),
            );
        }
        if self.inspector_open {
            columns.inspector = clamp_panel_width(
                columns.inspector,
                MIN_INSPECTOR_WIDTH,
                body_panel_max(available_width, columns.sidebar).min(MAX_INSPECTOR_WIDTH),
            );
        }
        columns
    }

    pub fn rows(self, available: f32) -> RowHeights {
        let available = available.max(0.0);
        if !self.execution_open {
            return RowHeights {
                coordinator: available,
                execution: 0.0,
            };
        }
        let coordinator = if available > MIN_COORDINATOR_HEIGHT + MIN_EXECUTION_HEIGHT {
            (available * self.coordinator_fraction)
                .clamp(MIN_COORDINATOR_HEIGHT, available - MIN_EXECUTION_HEIGHT)
        } else {
            available * DEFAULT_COORDINATOR_FRACTION
        };
        RowHeights {
            coordinator,
            execution: available - coordinator,
        }
    }

    pub fn resize_sidebar(&mut self, width: f32) {
        self.sidebar_width = width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    }

    pub fn resize_inspector(&mut self, width: f32) {
        self.inspector_width = width.clamp(MIN_INSPECTOR_WIDTH, MAX_INSPECTOR_WIDTH);
    }

    pub fn resize_sidebar_for_body(&mut self, width: f32, available_width: f32) {
        let inspector = if self.inspector_open {
            self.inspector_width
        } else {
            0.0
        };
        let max = body_panel_max(available_width, inspector).min(MAX_SIDEBAR_WIDTH);
        self.sidebar_width = clamp_panel_width(width, MIN_SIDEBAR_WIDTH, max);
    }

    pub fn resize_inspector_for_body(&mut self, width: f32, available_width: f32) {
        let sidebar = if self.sidebar_open {
            self.sidebar_width
        } else {
            0.0
        };
        let max = body_panel_max(available_width, sidebar).min(MAX_INSPECTOR_WIDTH);
        self.inspector_width = clamp_panel_width(width, MIN_INSPECTOR_WIDTH, max);
    }

    pub fn resize_coordinator(&mut self, height: f32, available: f32) {
        if available > 0.0 {
            let clamped = height.clamp(
                MIN_COORDINATOR_HEIGHT.min(available),
                (available - MIN_EXECUTION_HEIGHT).max(0.0),
            );
            self.coordinator_fraction = clamped / available;
        }
    }

    pub fn reset_sidebar(&mut self) {
        self.sidebar_width = DEFAULT_SIDEBAR_WIDTH;
    }

    pub fn reset_inspector(&mut self) {
        self.inspector_width = DEFAULT_INSPECTOR_WIDTH;
    }

    pub fn reset_workbench(&mut self) {
        self.coordinator_fraction = DEFAULT_COORDINATOR_FRACTION;
    }

    pub fn coordinator_fraction(self) -> f32 {
        self.coordinator_fraction
    }
}

fn body_panel_max(available_width: f32, other_panel_width: f32) -> f32 {
    (body_panel_budget(available_width) - other_panel_width.max(0.0)).max(0.0)
}

fn body_panel_budget(available_width: f32) -> f32 {
    (available_width.max(0.0) - MIN_CENTER_WIDTH).max(0.0)
}

fn clamp_panel_width(width: f32, min: f32, max: f32) -> f32 {
    if max < min {
        max
    } else {
        width.clamp(min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_approved_cockpit() {
        let layout = ShellLayout::default();
        assert_eq!(
            DEFAULT_INSPECTOR_WIDTH,
            crate::theme::Metrics::INSPECTOR_WIDTH
        );
        assert_eq!(
            layout.columns(),
            ColumnWidths {
                sidebar: 300.0,
                inspector: 379.0,
            }
        );
        assert_eq!(
            layout.rows(700.0),
            RowHeights {
                coordinator: 385.0,
                execution: 315.0,
            }
        );
    }

    #[test]
    fn default_columns_match_reference_proportions_at_1280_logical_width() {
        let layout = ShellLayout::default();
        let columns = layout.columns_for_body(1280.0);
        let center = 1280.0 - columns.sidebar - columns.inspector;

        assert!((columns.sidebar / 1280.0 - 0.234).abs() < 0.003);
        assert!((center / 1280.0 - 0.470).abs() < 0.004);
        assert!((columns.inspector / 1280.0 - 0.296).abs() < 0.003);
        assert!((layout.coordinator_fraction() - 0.55).abs() < 0.001);
    }

    #[test]
    fn closed_panels_take_no_layout_space() {
        let layout = ShellLayout {
            sidebar_open: false,
            inspector_open: false,
            execution_open: false,
            ..ShellLayout::default()
        };
        assert_eq!(
            layout.columns(),
            ColumnWidths {
                sidebar: 0.0,
                inspector: 0.0,
            }
        );
        assert_eq!(
            layout.rows(700.0),
            RowHeights {
                coordinator: 700.0,
                execution: 0.0,
            }
        );
    }

    #[test]
    fn pointer_resizes_are_clamped() {
        let mut layout = ShellLayout::default();
        layout.resize_sidebar(900.0);
        layout.resize_inspector(20.0);
        layout.resize_coordinator(690.0, 700.0);
        assert_eq!(
            layout.columns(),
            ColumnWidths {
                sidebar: 400.0,
                inspector: 300.0,
            }
        );
        assert_eq!(
            layout.rows(700.0),
            RowHeights {
                coordinator: 550.0,
                execution: 150.0,
            }
        );
    }

    #[test]
    fn reset_methods_restore_default_widths_and_fraction_after_resize() {
        let mut layout = ShellLayout::default();
        layout.resize_sidebar(399.0);
        layout.resize_inspector(301.0);
        layout.resize_coordinator(300.0, 700.0);

        assert_ne!(layout.columns().sidebar, DEFAULT_SIDEBAR_WIDTH);
        assert_ne!(layout.columns().inspector, DEFAULT_INSPECTOR_WIDTH);
        assert_ne!(layout.coordinator_fraction(), DEFAULT_COORDINATOR_FRACTION);

        layout.reset_sidebar();
        layout.reset_inspector();
        layout.reset_workbench();

        assert_eq!(layout.columns().sidebar, DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(layout.columns().inspector, DEFAULT_INSPECTOR_WIDTH);
        assert_eq!(layout.coordinator_fraction(), DEFAULT_COORDINATOR_FRACTION);
        assert_eq!(layout.rows(700.0), ShellLayout::default().rows(700.0));
    }

    #[test]
    fn narrow_column_projection_does_not_rewrite_default_width_intent() {
        let layout = ShellLayout::default();

        assert_eq!(
            layout.columns_for_body(600.0),
            ColumnWidths {
                sidebar: 240.0,
                inspector: 0.0,
            }
        );
        assert_eq!(
            layout.columns_for_body(1_200.0),
            ColumnWidths {
                sidebar: DEFAULT_SIDEBAR_WIDTH,
                inspector: DEFAULT_INSPECTOR_WIDTH,
            }
        );
        assert_eq!(
            layout.columns(),
            ColumnWidths {
                sidebar: DEFAULT_SIDEBAR_WIDTH,
                inspector: DEFAULT_INSPECTOR_WIDTH,
            }
        );
    }

    #[test]
    fn narrow_column_projection_does_not_rewrite_custom_width_intent() {
        let mut layout = ShellLayout::default();
        layout.resize_sidebar(390.0);
        layout.resize_inspector(500.0);

        assert_eq!(
            layout.columns_for_body(900.0),
            ColumnWidths {
                sidebar: 390.0,
                inspector: 150.0,
            }
        );
        assert_eq!(
            layout.columns_for_body(1_600.0),
            ColumnWidths {
                sidebar: 390.0,
                inspector: 500.0,
            }
        );
        assert_eq!(
            layout.columns(),
            ColumnWidths {
                sidebar: 390.0,
                inspector: 500.0,
            }
        );
    }
}
