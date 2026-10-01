# b.to_s.size < 1000  <=>  b < 10**999 (same predicate, no string per step)
lim = 10 ** 999
a = 1
b = 1
i = 2
while b < lim
  a, b = b, a + b
  i += 1
end
puts i
