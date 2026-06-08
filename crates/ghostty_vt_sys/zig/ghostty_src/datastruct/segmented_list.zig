const std = @import("std");
const Allocator = std.mem.Allocator;

pub fn SegmentedList(comptime T: type, comptime prealloc: usize) type {
    return struct {
        const Self = @This();

        inline_items: [prealloc]T = undefined,
        extra: std.ArrayList(T) = .empty,
        len: usize = 0,

        pub fn deinit(self: *Self, alloc: Allocator) void {
            self.extra.deinit(alloc);
            self.* = undefined;
        }

        pub fn count(self: Self) usize {
            return self.len;
        }

        pub fn addOne(self: *Self, alloc: Allocator) Allocator.Error!*T {
            if (self.len < prealloc) {
                const ptr = &self.inline_items[self.len];
                self.len += 1;
                return ptr;
            }

            try self.extra.append(alloc, undefined);
            self.len += 1;
            return &self.extra.items[self.len - prealloc - 1];
        }

        pub fn append(self: *Self, alloc: Allocator, item: T) Allocator.Error!void {
            const ptr = try self.addOne(alloc);
            ptr.* = item;
        }

        pub fn at(self: *Self, index: usize) *T {
            if (index < prealloc) return &self.inline_items[index];
            return &self.extra.items[index - prealloc];
        }

        pub fn constAt(self: *const Self, index: usize) *const T {
            if (index < prealloc) return &self.inline_items[index];
            return &self.extra.items[index - prealloc];
        }

        pub fn growCapacity(self: *Self, alloc: Allocator, new_capacity: usize) Allocator.Error!void {
            if (new_capacity <= prealloc) return;
            try self.extra.ensureTotalCapacity(alloc, new_capacity - prealloc);
        }

        pub fn iterator(self: *Self, start: usize) Iterator {
            return .{ .list = self, .index = start };
        }

        pub fn constIterator(self: *const Self, start: usize) ConstIterator {
            return .{ .list = self, .index = start };
        }

        pub const Iterator = struct {
            list: *Self,
            index: usize,

            pub fn next(self: *Iterator) ?*T {
                if (self.index >= self.list.len) return null;
                defer self.index += 1;
                return self.list.at(self.index);
            }
        };

        pub const ConstIterator = struct {
            list: *const Self,
            index: usize,

            pub fn next(self: *ConstIterator) ?*const T {
                if (self.index >= self.list.len) return null;
                defer self.index += 1;
                return self.list.constAt(self.index);
            }
        };
    };
}
