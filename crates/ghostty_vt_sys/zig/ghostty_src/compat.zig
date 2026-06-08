const std = @import("std");

pub fn intToEnum(comptime E: type, value: anytype) error{InvalidEnumTag}!E {
    const info = @typeInfo(E).@"enum";
    const int_value = std.math.cast(info.tag_type, value) orelse return error.InvalidEnumTag;

    inline for (info.fields) |field| {
        if (field.value == int_value) return @enumFromInt(int_value);
    }

    return error.InvalidEnumTag;
}
