---@meta

---@class MluaJsonNull

---@class MluaStrictJson
---@field null MluaJsonNull
---@field encode fun(value: any): string
---@field decode fun(value: string): any
---@field array fun(value: table): table
---@field object fun(value: table): table
---@field kind fun(value: any): 'array'|'object'|'null'|nil
