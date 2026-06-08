const std = @import("std");
const Target = @import("target.zig").Target;

pub fn Struct(
    comptime target: Target,
    comptime Zig: type,
) type {
    return switch (target) {
        .zig => Zig,
        .c => c: {
            const info = @typeInfo(Zig).@"struct";
            var names: [info.fields.len][:0]const u8 = undefined;
            var types: [info.fields.len]type = undefined;
            var attrs: [info.fields.len]std.builtin.Type.StructField.Attributes = undefined;
            for (info.fields, 0..) |field, i| {
                names[i] = field.name;
                types[i] = field.type;
                attrs[i] = .{
                    .default_value_ptr = field.default_value_ptr,
                    .@"comptime" = field.is_comptime,
                    .@"align" = field.alignment,
                };
            }

            break :c @Struct(.@"extern", null, &names, &types, &attrs);
        },
    };
}
