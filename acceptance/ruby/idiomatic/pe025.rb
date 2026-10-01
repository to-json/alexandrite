a, b, i = 1, 1, 2
while b.to_s.size < 1000 do a, b, i = b, a + b, i + 1 end
puts i
