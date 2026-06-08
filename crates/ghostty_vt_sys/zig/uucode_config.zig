const std = @import("std");
const config = @import("config.zig");

const Allocator = std.mem.Allocator;

pub const fields = &config.mergeFields(config.fields, &.{
    .{ .name = "width", .type = u2 },
    .{ .name = "is_symbol", .type = bool },
});

pub const build_components = &config.mergeComponents(config.build_components, &.{
    .{
        .Impl = Width,
        .inputs = &.{
            "wcwidth_standalone",
            "wcwidth_zero_in_grapheme",
            "is_emoji_modifier",
            "grapheme_break_no_control",
        },
        .fields = &.{"width"},
    },
    .{
        .Impl = IsSymbol,
        .inputs = &.{ "block", "general_category" },
        .fields = &.{"is_symbol"},
    },
});

pub const get_components = config.get_components;

pub const tables: []const config.Table = &.{
    .{
        .name = "runtime",
        .fields = &.{
            "is_emoji_presentation",
            "case_folding_full",
        },
    },
    .{
        .name = "buildtime",
        .fields = &.{
            "width",
            "wcwidth_zero_in_grapheme",
            "grapheme_break_no_control",
            "is_symbol",
            "is_emoji_vs_base",
        },
    },
};

const Width = struct {
    pub fn build(
        comptime InputRow: type,
        comptime Row: type,
        allocator: Allocator,
        io: std.Io,
        inputs: config.MultiSlice(InputRow),
        rows: *config.MultiSlice(Row),
        backing: anytype,
        tracking: anytype,
    ) !void {
        _ = allocator;
        _ = io;
        _ = backing;
        _ = tracking;

        rows.len = config.num_code_points;
        const items = rows.items(.width);
        const standalone_items = inputs.items(.wcwidth_standalone);
        const zero_in_grapheme_items = inputs.items(.wcwidth_zero_in_grapheme);
        const emoji_modifier_items = inputs.items(.is_emoji_modifier);
        const grapheme_items = inputs.items(.grapheme_break_no_control);

        for (0..config.num_code_points) |i| {
            if (zero_in_grapheme_items[i] and
                !emoji_modifier_items[i] and
                grapheme_items[i] != .prepend)
            {
                items[i] = 0;
            } else {
                items[i] = @min(2, standalone_items[i]);
            }
        }
    }
};

const IsSymbol = struct {
    pub fn build(
        comptime InputRow: type,
        comptime Row: type,
        allocator: Allocator,
        io: std.Io,
        inputs: config.MultiSlice(InputRow),
        rows: *config.MultiSlice(Row),
        backing: anytype,
        tracking: anytype,
    ) !void {
        _ = allocator;
        _ = io;
        _ = backing;
        _ = tracking;

        rows.len = config.num_code_points;
        const items = rows.items(.is_symbol);
        const block_items = inputs.items(.block);
        const category_items = inputs.items(.general_category);

        for (0..config.num_code_points) |i| {
            const block = block_items[i];
            items[i] = category_items[i] == .other_private_use or
                block == .arrows or
                block == .dingbats or
                block == .emoticons or
                block == .miscellaneous_symbols or
                block == .enclosed_alphanumerics or
                block == .enclosed_alphanumeric_supplement or
                block == .miscellaneous_symbols_and_pictographs or
                block == .transport_and_map_symbols;
        }
    }
};
