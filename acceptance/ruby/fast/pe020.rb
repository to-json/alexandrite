f = 1
i = 2
while i <= 100
  f *= i
  i += 1
end
s = f.to_s
puts s.sum(0) - 48 * s.bytesize
