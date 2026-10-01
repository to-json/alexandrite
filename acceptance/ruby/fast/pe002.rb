a = 1
b = 2
s = 0
while a <= 4_000_000
  s += a if a.even?
  a, b = b, a + b
end
puts s
