-- diff harness sample: lua
local function fib(n)
  if n <= 1 then
    return n
  elseif n == 2 then
    return 1
  else
    return fib(n - 1) + fib(n - 2)
  end
end
for i = 1, 10 do
  while i > 0 do
    i = i - 1
  end
end
repeat
  print("x")
until true
local s = [[
long string
]]
if true then
  do
    print(1)
  end
end
