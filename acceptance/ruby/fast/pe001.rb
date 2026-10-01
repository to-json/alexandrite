s = 0
i = 1
while i < 1000
  s += i if i % 3 == 0 || i % 5 == 0
  i += 1
end
puts s
